//! Read-only application projections of the effective context and recorded work.

use super::*;
use crate::capabilities::progress_guard::{ToolClass, classify_tool_call};
use anyhow::Context;
use everruns_core::{ContentPart, EventData};

pub(crate) struct InspectionEntry {
    pub title: String,
    pub detail: String,
    artifacts: Vec<String>,
}

fn bounded(text: &str) -> String {
    const LIMIT: usize = 64 * 1024;
    if text.len() <= LIMIT {
        text.to_owned()
    } else {
        format!(
            "{}\n[Preview limited to 64 KiB]",
            &text[..text.floor_char_boundary(LIMIT)]
        )
    }
}

fn entry(title: impl Into<String>, detail: impl Into<String>) -> InspectionEntry {
    InspectionEntry {
        title: title.into(),
        detail: bounded(&detail.into()),
        artifacts: Vec::new(),
    }
}

fn context_text(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in parts {
        match part {
            ContentPart::Text(part) => text.push_str(&part.text),
            ContentPart::Image(_) | ContentPart::ImageFile(_) => {
                text.push_str("[Image attachment, payload omitted from text preview]")
            }
            ContentPart::ProviderOpaque(_) => text.push_str("[Provider replay artifact]"),
            other => text.push_str(&serde_json::to_string(other).unwrap_or_default()),
        }
        text.push('\n');
        if text.len() >= 64 * 1024 {
            break;
        }
    }
    bounded(&text)
}

impl Session {
    async fn inspect_instruction_contribution(&self) -> Result<String> {
        use everruns::hosting::SessionHosting;
        use everruns_core::{Capability, SystemPromptContext};
        // Framework SessionContext exposes filtered history but not the
        // TurnAgent's dynamic conversation contribution. Resolve this one
        // through its native capability and the live broker, using retained
        // behavior plus the current session overlay, never a separate scanner.
        let profile = &self.handles.host.profile;
        let snapshot = profile.query_snapshot(self.session_id()).await?;
        let mut binding = snapshot.binding;
        for _ in 0..32 {
            if binding.metadata.get("yolop_build").is_some() {
                break;
            }
            let Some(parent) = binding
                .metadata
                .get("parent_session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|id| id.parse().ok())
            else {
                break;
            };
            binding = profile.status(parent).await?.binding;
        }
        let mut capabilities: Vec<everruns_contracts::CapabilityRef> = serde_json::from_value(
            binding
                .metadata
                .get("yolop_build")
                .and_then(|value| value.get("capabilities"))
                .cloned()
                .unwrap_or_default(),
        )?;
        capabilities.extend(snapshot.configuration.capabilities.unwrap_or_default());
        let Some(capability) = capabilities
            .iter()
            .rev()
            .find(|capability| capability.id() == "agent_instructions")
        else {
            return Ok(String::new());
        };
        let mut context = SystemPromptContext::without_file_store(self.session_id());
        context.file_store = Some(self.handles.host.files.clone());
        Ok(everruns_core::builtins::AgentInstructionsCapability
            .conversation_context_contribution_with_config(&context, capability.config_value())
            .await
            .unwrap_or_default())
    }

    pub(crate) async fn inspect_context(&self) -> Result<Vec<InspectionEntry>> {
        let context = self.handles.session.command_context().await?;
        let mut rows = vec![entry(
            format!(
                "Effective system instructions · ~{} tokens",
                context.instructions.chars().count().div_ceil(4)
            ),
            format!(
                "Source: runtime-assembled system context. Token sizes are character/4 estimates.\n\n{}",
                context.instructions
            ),
        )];
        if let Some((_, catalog)) = context.instructions.split_once("Available skills:") {
            rows.push(entry("Available skills, catalog only", format!("Source: active runtime system prompt. Discovery lists descriptions, not loaded skill bodies.\n\n{}", bounded(catalog))));
        }
        let instructions = regex::Regex::new(
            r#"(?s)<agent-instructions source="([^"]+)">\s*(.*?)</agent-instructions>"#,
        )?;
        let mut sources = HashSet::new();
        match self.inspect_instruction_contribution().await {
            Ok(text) => {
                for captures in instructions.captures_iter(&text) {
                    let source = html_escape::decode_html_entities(&captures[1]).to_string();
                    if sources.insert(source.clone()) {
                        rows.push(entry(format!("Loaded instruction file: {source}"),format!("Source: {source}\nLive next-request conversation contribution from the native instruction capability. It is reread each turn, rather than stored as system instructions.\n\n{}",html_escape::decode_html_entities(&captures[2]))));
                    }
                }
            }
            Err(error) => rows.push(entry(
                "Instruction contribution unavailable",
                format!("{error:#}. System instructions and retained context remain inspectable."),
            )),
        }
        let skill_calls: HashSet<_> = context
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|part| match part {
                ContentPart::ToolCall(call) if call.name == "activate_skill" => {
                    Some(call.id.as_str())
                }
                _ => None,
            })
            .collect();
        let skills = regex::Regex::new(r#"(?s)<skill name="([^"]+)">\s*(.*?)</skill>"#)?;
        let mut loaded_skills = HashSet::new();
        let mut messages = Vec::new();
        for message in context.messages.iter().rev().take(128) {
            let text = context_text(&message.content);
            for part in &message.content {
                let ContentPart::ToolResult(part) = part else {
                    continue;
                };
                if !skill_calls.contains(part.tool_call_id.as_str()) {
                    continue;
                }
                let Some(result) = &part.result else {
                    continue;
                };
                let Some(body) = result["instructions"].as_str() else {
                    continue;
                };
                for captures in skills.captures_iter(body) {
                    if loaded_skills.insert(captures[1].to_owned()) {
                        rows.push(entry(format!("Loaded skill: {}", &captures[1]), format!("Source: activate_skill result in retained live context. Scope: {}.\n\n{}", result["scope"].as_str().unwrap_or("not reported"), &captures[2])));
                    }
                }
            }
            messages.push(entry(format!("{:?} · ~{} tokens · {}", message.role, text.chars().count().div_ceil(4),text.lines().next().unwrap_or_default().chars().take(90).collect::<String>()),format!("Source: filtered live model context. Masked output and retained summaries appear as the runtime sends them.\n\n{text}")));
        }
        rows.push(entry(
            format!("Model-visible tools · {}", context.tools.len()),
            context
                .tools
                .iter()
                .map(|tool| format!("{}: {}", tool.name, tool.description))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        rows.extend(messages);
        if context.messages.len() > 128 {
            rows.push(entry("Context preview limit",format!("Showing the most recent 128 of {} filtered messages. Each text preview is bounded to 64 KiB.",context.messages.len())));
        }
        for warning in context.plugin_warnings {
            rows.push(entry("Runtime context warning", warning));
        }
        Ok(rows)
    }

    pub(crate) async fn inspect_checks(
        &self,
        root: &std::path::Path,
    ) -> Result<Vec<InspectionEntry>> {
        let events = self.handles.host.events().await?;
        let current = crate::exec::workspace_evidence::capture(root).await.ok();
        let rows = check_entries(&events, current.as_ref());
        let root = self
            .handles
            .checkpoints
            .sessions_dir()
            .join(self.session_id().to_string());
        tokio::task::spawn_blocking(move || {
            Ok(rows
                .into_iter()
                .map(|mut row| {
                    row.detail
                        .push_str(&crate::exec::private_artifact::previews(
                            &root,
                            row.artifacts.clone(),
                        ));
                    row
                })
                .collect())
        })
        .await?
    }

    pub(crate) async fn inspect_background_checks(
        &self,
        tasks: Vec<everruns_core::SessionTask>,
        root: &std::path::Path,
    ) -> Result<Vec<InspectionEntry>> {
        let current = crate::exec::workspace_evidence::capture(root).await.ok();
        let sessions = self.handles.checkpoints.sessions_dir().to_path_buf();
        tokio::task::spawn_blocking(move || {
            tasks.into_iter().take(100).map(|task| {
                let directory = sessions.join(task.session_id.to_string());
                let text = task.result_path.as_deref().map(|path| crate::exec::private_artifact::read(&directory,path).unwrap_or_else(|error| format!("Saved result unavailable: {error:#}"))).unwrap_or_else(|| "No saved result yet; inspect the task for live output.".into());
                let result = serde_json::from_str(&text).unwrap_or_default();
                let freshness = evidence_state(&result,current.as_ref(),false);
                let command = task.spec["arguments"]["command"].as_str().unwrap_or("unknown command");
                let mut row = entry(format!("background {} · {freshness} · {command}",task.state),
                    format!("Command: {command}\nTask: {}\nOwner: {}\nStarted: {:?}\nFinished: {:?}\nFreshness: {freshness}\nExit code: {}\n\n{text}",task.id,task.session_id,task.started_at,task.finished_at,result["exit_code"]));
                row.detail.push_str(&crate::exec::private_artifact::previews(&directory,task.result_path.into_iter().collect()));
                Ok(row)
            }).collect()
        }).await?
    }

    pub(crate) async fn inspect_child_transcript(&self, child: SessionId) -> Result<String> {
        let events = self.handles.host.session_events(child).await?;
        let lines = events
            .iter()
            .rev()
            .take(200)
            .rev()
            .flat_map(lines_for_replayed_event)
            .map(|line| format!("{}: {}", line.author.label(), bounded(&line.text)))
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(format!("\n\nRecent agent transcript:\n{}", bounded(&lines)))
    }

    pub(crate) async fn message_task(
        &self,
        owner: SessionId,
        task_id: &str,
        message: &str,
        in_reply_to: Option<&str>,
    ) -> Result<String> {
        use everruns::hosting::SessionHosting;
        use everruns_capabilities::capabilities::session_tasks::MessageTaskTool;
        anyhow::ensure!(
            !message.trim().is_empty() && message.len() <= 4096,
            "Task messages must contain 1–4096 bytes"
        );
        let mut cursor = owner;
        for depth in 0..=32 {
            if cursor == self.session_id() {
                break;
            }
            anyhow::ensure!(
                depth < 32,
                "Task owner is outside this session's delegation tree"
            );
            let record = self
                .handles
                .session_store
                .get_session(cursor)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Task owner no longer exists"))?;
            cursor = record
                .parent_session_id
                .or(record.forked_from_session_id)
                .ok_or_else(|| {
                    anyhow::anyhow!("Task owner is outside this session's delegation tree")
                })?;
        }
        let backends = self.handles.host.profile.reader(owner);
        let mut context = everruns_core::ToolContext::new(owner);
        context.session_task_registry = backends.session_task_registry;
        if let Some(reply) = in_reply_to {
            let task = context
                .session_task_registry
                .as_ref()
                .context("task registry unavailable")?
                .get(owner, task_id)
                .await?
                .context("task no longer exists")?;
            anyhow::ensure!(
                task.input_request
                    .as_ref()
                    .is_some_and(|request| request.id == reply),
                "The agent's input request changed. Inspect it before answering again."
            );
        }
        context.session_store = Some(self.handles.session_store.clone());
        if let Some(factory) = backends.tool_context_extensions_factory {
            context.extensions = factory(
                everruns_core::host::in_process_internal_org_id(
                    everruns_core::DEFAULT_ORG_PUBLIC_ID,
                ),
                owner,
            );
        }
        let result = MessageTaskTool
            .execute_with_context(
                serde_json::json!({"task_id":task_id,"message":message,"in_reply_to":in_reply_to}),
                &context,
            )
            .await;
        match result {
            everruns_core::ToolExecutionResult::Success(value) => Ok(format!(
                "Message recorded. Delivery: {}.\n{}",
                value["delivery"].as_str().unwrap_or("not reported"),
                serde_json::to_string_pretty(&value)?
            )),
            everruns_core::ToolExecutionResult::ToolError(error) => anyhow::bail!("{error}"),
            _ => anyhow::bail!(
                "Task message could not be delivered; inspect the task channel before retrying"
            ),
        }
    }
}

fn check_entries(
    events: &[everruns_core::Event],
    current: Option<&crate::exec::workspace_evidence::WorkspaceEvidence>,
) -> Vec<InspectionEntry> {
    let mut calls = std::collections::HashMap::new();
    let mut checks = Vec::new();
    let mut last_mutation = None;
    for (index, event) in events.iter().enumerate() {
        match &event.data {
            EventData::ToolStarted(started) => {
                let class = classify_tool_call(&started.tool_call);
                if matches!(class, ToolClass::Mutation)
                    || (matches!(class, ToolClass::Other) && started.tool_call.name == "bash")
                {
                    last_mutation = Some(index);
                }
                calls.insert(started.tool_call.id.clone(), started.tool_call.clone());
            }
            EventData::ToolCompleted(completed) => {
                let Some(mut call) = calls.remove(&completed.tool_call_id) else {
                    continue;
                };
                if let Some(arguments) = &completed.executed_arguments {
                    call.arguments = arguments.clone();
                }
                if matches!(classify_tool_call(&call), ToolClass::Validation(_)) {
                    checks.push((index, event, completed, call));
                }
            }
            _ => {}
        }
    }
    checks.into_iter().rev().take(100).map(|(index, event, completed, call)| {
        let result = crate::tui::transcript::result_value(completed).unwrap_or_default();
        let exit = result["exit_code"].as_i64();
        let succeeded = completed.success && exit == Some(0)
            && ["timed_out", "cancelled", "output_limited"].iter().all(|flag| result[*flag].as_bool() != Some(true))
            && result["success"].as_bool() != Some(false);
        let after = serde_json::from_value::<crate::exec::workspace_evidence::WorkspaceEvidence>(result["workspace_evidence"]["after"].clone()).ok();
        let state = evidence_state(&result,current,last_mutation.is_some_and(|mutation|mutation > index));
        let revision = after.as_ref().map(|stamp| format!("{} · workspace {}", stamp.head, stamp.fingerprint)).unwrap_or_else(|| "not recorded for this historical command".into());
        let command = call.arguments["command"].as_str().unwrap_or(&call.name);
        let mut row = entry(format!("{} · {state} · {}", if succeeded { "passed" } else if exit.is_some() { "failed" } else { "outcome unknown" }, command.chars().take(150).collect::<String>()),
            format!("Command: {command}\nWorkspace scope: Git-visible session workspace files\nRecorded: {}\nExit code: {}\nDuration: {}\nFreshness: {state}\nRevision: {revision}\nIgnored files, submodules and external dependencies are outside this fingerprint. Historical records without fingerprints remain unverified.\n\n{}\n{}", event.ts, exit.map(|code| code.to_string()).unwrap_or_else(|| "not reported".into()), completed.duration_ms.map(|duration| format!("{duration} ms")).unwrap_or_else(|| "unknown".into()), serde_json::to_string_pretty(&result).unwrap_or_default(), completed.error.as_deref().unwrap_or_default()));
        row.artifacts = crate::exec::private_artifact::references(&result);
        row
    }).collect()
}

fn evidence_state(
    result: &serde_json::Value,
    current: Option<&crate::exec::workspace_evidence::WorkspaceEvidence>,
    observed_writes: bool,
) -> &'static str {
    use crate::exec::workspace_evidence::WorkspaceEvidence;
    let before =
        serde_json::from_value::<WorkspaceEvidence>(result["workspace_evidence"]["before"].clone())
            .ok();
    let after =
        serde_json::from_value::<WorkspaceEvidence>(result["workspace_evidence"]["after"].clone())
            .ok();
    match (before.as_ref(), after.as_ref(), current) {
        (Some(before), Some(after), _) if before != after => {
            "stale, workspace changed during check"
        }
        (_, Some(after), Some(current)) if after != current => {
            "stale, workspace changed after check"
        }
        (Some(_), Some(_), Some(_)) => "current Git-visible workspace snapshot",
        _ if observed_writes => "stale after observed writes",
        _ => "freshness unknown, no workspace fingerprint",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::{Event, EventContext, ToolCompletedData, ToolStartedData};
    #[test]
    fn recorded_checks_report_actual_exit_duration_revision_and_stale_or_unknown_evidence() {
        let id = SessionId::new();
        let stamp = crate::exec::workspace_evidence::WorkspaceEvidence {
            head: "a".repeat(40),
            fingerprint: "b".repeat(64),
        };
        let call = everruns_contracts::ToolCall {
            id: "check".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":"cargo test"}),
        };
        let start = Event::new(
            id,
            EventContext::empty(),
            ToolStartedData {
                tool_call: call,
                tool_call_fingerprint: None,
                display_name: None,
                narration: None,
                side_effect_class: None,
            },
        );
        let mut completed = ToolCompletedData::success("check".into(),"bash".into(),vec![ContentPart::text(serde_json::json!({"exit_code":0,"success":true,"workspace_evidence":{"before":stamp,"after":stamp}}).to_string())],None);
        completed.duration_ms = Some(321);
        let finish = Event::new(id, EventContext::empty(), completed);
        let events = vec![start, finish];
        let rows = check_entries(&events, Some(&stamp));
        assert!(rows[0].title.starts_with("passed · current"));
        assert!(rows[0].detail.contains("321 ms"));
        assert!(rows[0].detail.contains(&stamp.head));
        let changed = crate::exec::workspace_evidence::WorkspaceEvidence {
            fingerprint: "c".repeat(64),
            ..stamp
        };
        assert!(
            check_entries(&events, Some(&changed))[0]
                .title
                .contains("changed after check")
        );
        assert!(
            check_entries(&events, None)[0]
                .title
                .contains("freshness unknown")
        );
        let mut failed = events;
        if let EventData::ToolCompleted(data) = &mut failed[1].data {
            data.result = Some(vec![ContentPart::text(
                r#"{"exit_code":1,"success":false}"#,
            )]);
        }
        assert!(check_entries(&failed, None)[0].title.starts_with("failed"));
    }
}

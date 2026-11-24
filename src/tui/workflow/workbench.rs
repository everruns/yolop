//! Acquisition and actions for the terminal workbench, using runtime authority.

use super::*;

impl App {
    pub(super) fn toggle_session_label(&mut self, archive: bool) {
        let Some(Action::Session(id)) = self.workflow.selected_row().map(|row| row.action.clone())
        else {
            return;
        };
        self.write_session_labels(
            id,
            if archive {
                LabelEdit::Archive
            } else {
                LabelEdit::Pin
            },
        );
    }

    pub(super) fn update_session_labels(&mut self, title: Option<String>) {
        let Some(id) = self.workflow.editing_session.take() else {
            return;
        };
        self.workflow.command_input = None;
        self.workflow.query.clear();
        self.write_session_labels(id, LabelEdit::Title(title));
    }

    fn write_session_labels(&mut self, id: SessionId, edit: LabelEdit) {
        let root = self
            .session
            .handles()
            .checkpoints
            .sessions_dir()
            .to_path_buf();
        self.workflow.cancel_load();
        self.workflow.pending = Some(tokio::task::spawn_blocking(move || {
            let mut labels = crate::runtime::session_labels::read(&root, id)?;
            let message = match edit {
                LabelEdit::Title(title) => {
                    labels.title = title;
                    "Session renamed"
                }
                LabelEdit::Pin => {
                    labels.pinned = !labels.pinned;
                    if labels.pinned {
                        "Session pinned"
                    } else {
                        "Session unpinned"
                    }
                }
                LabelEdit::Archive => {
                    labels.archived = !labels.archived;
                    if labels.archived {
                        "Session archived, available in the Archived view"
                    } else {
                        "Session restored to Active"
                    }
                }
            };
            crate::runtime::session_labels::write(&root, id, &labels)?;
            Ok(Loaded::SessionChanged(message.into()))
        }));
    }

    pub(in crate::tui) fn open_workbench(&mut self, panel: WorkflowPanel) {
        self.workflow.cancel_load();
        self.workflow.refreshed = Instant::now();
        match panel {
            WorkflowPanel::Checks => {
                let session = self.session.clone();
                let root = self.worktree.active_root();
                let tasks = self
                    .session_tasks
                    .rows
                    .iter()
                    .filter(|row| {
                        row.task.spec["tool"].as_str() == Some("bash")
                            && row.task.spec["arguments"]["command"]
                                .as_str()
                                .is_some_and(crate::capabilities::progress_guard::is_check_command)
                    })
                    .map(|row| row.task.clone())
                    .collect();
                self.workflow.pending = Some(tokio::spawn(async move {
                    let mut rows: Vec<_> = session
                        .inspect_checks(&root)
                        .await?
                        .into_iter()
                        .map(|entry| Row {
                            label: entry.title,
                            detail: entry.detail,
                            action: Action::None,
                        })
                        .collect();
                    match session.inspect_background_checks(tasks, &root).await {
                        Ok(background) => rows.extend(background.into_iter().map(|entry| Row {
                            label: entry.title,
                            detail: entry.detail,
                            action: Action::None,
                        })),
                        Err(error) => rows.push(note(
                            "Background evidence unavailable",
                            &format!("{error:#}. Open /tasks to inspect individual work."),
                        )),
                    }
                    if rows.is_empty() {
                        rows.push(note("No recorded checks", "Validation commands executed in this session appear here with their actual exit codes, output and workspace fingerprints. No checks have been recorded on the active history branch."));
                    }
                    Ok(Loaded::Rows(rows))
                }));
            }
            WorkflowPanel::Files => {
                let root = self.worktree.active_root();
                self.workflow.pending = Some(tokio::spawn(async move {
                    let paths = match super::super::review::git(
                        &root,
                        &[
                            "ls-files",
                            "--cached",
                            "--others",
                            "--exclude-standard",
                            "-z",
                            "--",
                            ".",
                        ],
                    )
                    .await
                    {
                        Ok(bytes) => bytes
                            .split(|byte| *byte == 0)
                            .filter_map(|path| std::str::from_utf8(path).ok())
                            .filter(|path| !path.is_empty())
                            .map(str::to_owned)
                            .take(10_000)
                            .collect(),
                        Err(_) => {
                            tokio::task::spawn_blocking(move || {
                                ignore::WalkBuilder::new(&root)
                                    .follow_links(false)
                                    .max_depth(Some(64))
                                    .build()
                                    .take(50_000)
                                    .filter_map(|entry| entry.ok())
                                    .filter(|entry| {
                                        entry.file_type().is_some_and(|kind| kind.is_file())
                                    })
                                    .filter_map(|entry| {
                                        entry
                                            .path()
                                            .strip_prefix(&root)
                                            .ok()?
                                            .to_str()
                                            .map(str::to_owned)
                                    })
                                    .take(10_000)
                                    .collect::<Vec<String>>()
                            })
                            .await?
                        }
                    };
                    let mut paths: Vec<String> = paths;
                    paths.sort();
                    paths.dedup();
                    Ok(Loaded::Rows(paths.into_iter().map(|path| Row {
                        label: super::super::review::file_display(&path),
                        detail: "Select a file to preview its content. Enter attaches a bounded snapshot; add #start-end to select lines. Repository discovery is limited to 10,000 files.".into(),
                        action: Action::AttachReference(path),
                    }).collect()))
                }));
            }
            WorkflowPanel::Tasks => {
                let rows: Vec<Row> = self
                    .session_tasks
                    .rows
                    .iter()
                    .map(|row| Row {
                        label: format!(
                            "{} · {} · {}",
                            row.task.display_name, row.task.kind, row.task.state
                        ),
                        detail: task_summary(&row.task),
                        action: Action::Task {
                            owner: row.task.session_id,
                            id: row.task.id.clone(),
                        },
                    })
                    .collect();
                let rows = rows
                    .into_iter()
                    .map(|mut row| {
                        if let Some(old) = self
                            .workflow
                            .rows
                            .iter()
                            .find(|old| row_key(old) == row_key(&row))
                        {
                            row.detail = old.detail.clone();
                        }
                        row
                    })
                    .collect();
                self.workflow.replace_rows(rows);
                for error in &self.session_tasks.errors {
                    self.workflow
                        .rows
                        .push(note("Task registry warning", error));
                }
                if self.workflow.rows.is_empty() {
                    self.workflow.rows.push(note(
                        "No background work",
                        "Running commands, agents, and scheduled monitors appear here.",
                    ));
                }
            }
            WorkflowPanel::Permissions => self.refresh_permissions(),
            WorkflowPanel::Attention => {
                let rows = self
                    .attention
                    .items
                    .iter()
                    .map(|notice| Row {
                        label: notice.title.clone(),
                        detail: notice.detail.clone(),
                        action: Action::Attention(notice.key.clone(), notice.target.clone()),
                    })
                    .collect();
                self.workflow.replace_rows(rows);
                if self.workflow.rows.is_empty() {
                    self.workflow.rows.push(note("Nothing needs attention", "Questions, approvals, failures, and completed work appear here. Delete acknowledges an item. Desktop alerts are opt-in for this process; n cycles Off, OSC 9 and OSC 777."));
                }
            }
            _ => {}
        }
    }

    pub(super) fn load_context(&mut self) {
        self.workflow.rows = vec![note(
            "Usage and occupancy",
            &present::usage_detail(&self.presentation_state()),
        )];
        let session = self.session.clone();
        let mut draft = self
            .pending_pastes
            .iter()
            .map(|(label, text)| Row {
                label: format!(
                    "Draft attachment: {label} · ~{} tokens",
                    text.chars().count().div_ceil(4)
                ),
                detail: format!(
                    "Source: unsent composer attachment. Included when you send the draft.\n\n{}",
                    bounded(text, 64 * 1024)
                ),
                action: Action::None,
            })
            .collect::<Vec<_>>();
        if !self.pending_images.is_empty() {
            draft.push(note("Draft images", &format!("{} image(s) attached to the unsent draft. Open /attachments to inspect or remove them.", self.pending_images.len())));
        }
        let summary = self.workflow.rows[0].clone();
        self.workflow.pending = Some(tokio::spawn(async move {
            let mut rows = vec![summary];
            rows.extend(draft);
            rows.extend(
                session
                    .inspect_context()
                    .await?
                    .into_iter()
                    .map(|entry| Row {
                        label: entry.title,
                        detail: entry.detail,
                        action: Action::None,
                    }),
            );
            Ok(Loaded::Rows(rows))
        }));
    }

    pub(super) fn load_file_preview(&mut self) {
        let Some(Action::AttachReference(path)) =
            self.workflow.selected_row().map(|row| row.action.clone())
        else {
            return;
        };
        let root = self.worktree.active_root();
        let reference = format!("{}{}", path, range_suffix(self.workflow.query.text()));
        self.workflow.cancel_load();
        self.workflow.pending = Some(tokio::task::spawn_blocking(move || {
            let text = match super::super::attachment::text(&root, &reference) {
                Ok((label, text)) => format!(
                    "{label}\nSnapshot preview, approximately {} tokens (characters/4).\n\n{text}",
                    text.chars().count().div_ceil(4)
                ),
                Err(error) => format!("Cannot attach {reference}: {error:#}"),
            };
            Ok(Loaded::FilePreview(path, text))
        }));
    }

    fn refresh_permissions(&mut self) {
        let handles = self.session.handles();
        let mode = handles.sandbox.mode();
        let state = self.presentation_state();
        self.workflow.rows = vec![note(
            "Effective execution boundary",
            &format!(
                "{}\n\nWorkspace: {}\nSandbox: {}\nNetwork: {}\nHard approval policy: {}\n\n{}\n\nSource: active process configuration; session grants below do not replace the sandbox policy.",
                state.approval_mode,
                self.worktree.active_root().display(),
                mode.as_str(),
                crate::exec::sandbox::network_access(mode),
                handles.approval_policy.as_str(),
                crate::exec::sandbox::access_summary(
                    handles.sandbox.as_ref(),
                    &self.worktree.active_root()
                )
            ),
        )];
        for scope in handles.sandbox_approval_gate.granted_scopes() {
            self.workflow.rows.push(Row { label: format!("Session grant: {}", scope.as_str()), detail: format!("All requests in {} have been permitted for this process session. Delete revokes this grant for future requests. Already running operations continue. Source: your earlier Allow for session decision.", scope.as_str()), action: Action::Revoke(scope) });
        }
        if self.workflow.rows.len() == 1 {
            self.workflow.rows.push(note(
                "No session grants",
                "Future consequential requests follow the active approval policy.",
            ));
        }
    }

    pub(super) fn inspect_task(&mut self, owner: SessionId, id: String) {
        let Some(task) = self
            .session_tasks
            .rows
            .iter()
            .find(|row| row.task.session_id == owner && row.task.id == id)
            .map(|row| row.task.clone())
        else {
            self.workflow.error =
                Some("Task is no longer in this session's activity tree. Refresh the list.".into());
            return;
        };
        self.workflow.inspected_task = Some((owner, id.clone()));
        let root = self
            .session
            .handles()
            .checkpoints
            .sessions_dir()
            .join(owner.to_string());
        let registry = self.task_registry.clone();
        let session = self.session.clone();
        self.workflow.cancel_load();
        self.workflow.pending = Some(tokio::spawn(async move {
            let mut detail = task_summary(&task);
            let messages = registry.list_messages(owner, &id, Some(50), None).await?;
            for message in messages {
                detail.push_str(&format!(
                    "\n\nMessage {:?}:\n{}",
                    message.direction,
                    bounded(&serde_json::to_string(&message.content)?, 16_384)
                ));
            }
            if let Some(child) = task.links.child_session_id {
                detail.push_str(
                    &session
                        .inspect_child_transcript(child)
                        .await
                        .unwrap_or_else(|error| {
                            format!("\n\nChild transcript unavailable: {error:#}")
                        }),
                );
            }
            let mut paths = task.result_path.clone().into_iter().collect::<Vec<_>>();
            for artifact in serde_json::to_value(&task.artifacts)?
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(path) = artifact["path"].as_str() {
                    paths.push(path.to_string());
                }
            }
            if let Some(result) = &task.result_path
                && let Some(parent) = Path::new(result).parent()
            {
                paths.insert(0, parent.join("output.log").to_string_lossy().into_owned());
            }
            let artifact_text = tokio::task::spawn_blocking(move || {
                crate::exec::private_artifact::previews(&root, paths)
            })
            .await?;
            detail.push_str(&artifact_text);
            Ok(Loaded::TaskDetail(owner, id, bounded(&detail, 512 * 1024)))
        }));
    }

    pub(super) fn begin_task_message(&mut self) {
        let Some(Action::Task { owner, id }) =
            self.workflow.selected_row().map(|row| row.action.clone())
        else {
            return;
        };
        let Some(task) = self
            .session_tasks
            .rows
            .iter()
            .find(|row| row.task.session_id == owner && row.task.id == id)
        else {
            return;
        };
        if task.task.links.child_session_id.is_none() {
            self.workflow.error = Some(
                "This task has no agent message channel. Inspect its output or cancel it instead."
                    .into(),
            );
            return;
        }
        self.workflow.message_reply = task
            .task
            .input_request
            .as_ref()
            .map(|request| request.id.clone());
        self.workflow.message_context = format!(
            "Agent: {}\nTask: {}\n{}",
            task.task.display_name,
            id,
            task.task
                .input_request
                .as_ref()
                .map(|request| format!("Answering request {}:\n{}", request.id, request.prompt))
                .unwrap_or_else(|| "Send an ordinary message to this agent.".into())
        );
        self.workflow.inspected_task = Some((owner, id));
        self.workflow.command_input = Some("task-message".into());
        self.workflow.query.clear();
    }

    pub(super) fn send_task_message(&mut self, message: String) {
        let Some((owner, id)) = self.workflow.inspected_task.clone() else {
            return;
        };
        self.workflow.command_input = None;
        self.workflow.query.clear();
        let session = self.session.clone();
        let reply = self.workflow.message_reply.take();
        self.workflow.pending = Some(tokio::spawn(async move {
            let detail = session
                .message_task(owner, &id, &message, reply.as_deref())
                .await?;
            Ok(Loaded::TaskMessage(owner, id, detail))
        }));
    }

    pub(in crate::tui) fn ask_side_question(&mut self, question: &str) {
        self.open_workflow(WorkflowPanel::SideQuestion);
        if self.workflow.panel != Some(WorkflowPanel::SideQuestion) {
            return;
        }
        if question.trim().is_empty() {
            if self.workflow.side_question.is_empty() {
                self.workflow.command_input = Some("btw".into());
            }
            return;
        }
        self.workflow.side_question = question.trim().to_owned();
        self.workflow.side_answer.clear();
        let question = self.workflow.side_question.clone();
        let session = self.session.clone();
        self.workflow.pending = Some(tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(120),
                session.execute_command("btw", Some(question)),
            )
            .await
            .context("Side question timed out")??;
            anyhow::ensure!(result.success, "{}", result.message);
            Ok(Loaded::SideAnswer(result.message))
        }));
    }
}

enum LabelEdit {
    Title(Option<String>),
    Pin,
    Archive,
}

fn note(label: &str, detail: &str) -> Row {
    Row {
        label: label.into(),
        detail: detail.into(),
        action: Action::None,
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_owned()
    } else {
        format!(
            "{}\n[Preview limited to {limit} bytes]",
            &text[..text.floor_char_boundary(limit)]
        )
    }
}

fn task_summary(task: &everruns_core::SessionTask) -> String {
    format!(
        "{}\nTask: {}\nOwner: {}\nKind: {}\nState: {}\nCreated: {}\nStarted: {}\nFinished: {}\nCancellation requested: {}\n\n{}\n{}\n\nInput request:\n{}\n\nInput:\n{}",
        task.display_name,
        task.id,
        task.session_id,
        task.kind,
        task.state,
        task.created_at,
        task.started_at
            .map(|time| time.to_string())
            .unwrap_or_else(|| "not started".into()),
        task.finished_at
            .map(|time| time.to_string())
            .unwrap_or_else(|| "still active".into()),
        task.cancel_requested_at.is_some(),
        task.state_detail.as_deref().unwrap_or_default(),
        task.summary.as_deref().unwrap_or_default(),
        task.input_request
            .as_ref()
            .map(|request| bounded(
                &serde_json::to_string_pretty(request).unwrap_or_default(),
                32_768
            ))
            .unwrap_or_else(|| "None".into()),
        bounded(
            &serde_json::to_string_pretty(&task.spec).unwrap_or_default(),
            32_768
        )
    )
}

pub(super) fn range_suffix(query: &str) -> &str {
    super::super::attachment::range_suffix(query)
}

pub(super) fn matching_files(rows: &[Row], query: &str) -> Vec<usize> {
    let suffix = range_suffix(query);
    let needle = query[..query.len() - suffix.len()]
        .trim()
        .trim_start_matches('@')
        .to_lowercase();
    let mut matches: Vec<_> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let Action::AttachReference(path) = &row.action else {
                return None;
            };
            fuzzy_score(&needle, &path.to_lowercase()).map(|score| (score, path.len(), index))
        })
        .collect();
    matches.sort_unstable();
    matches
        .into_iter()
        .take(100)
        .map(|(_, _, index)| index)
        .collect()
}

fn fuzzy_score(needle: &str, path: &str) -> Option<usize> {
    let mut remaining = path;
    let mut score = 0;
    for ch in needle.chars() {
        let index = remaining.find(ch)?;
        score += index;
        remaining = &remaining[index + ch.len_utf8()..];
    }
    Some(score)
}

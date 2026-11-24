//! Yolop workflow composition over tuika's existing views and input models.

use super::review::{ReviewScope, ReviewSnapshot};
use super::workflow_presentation::{self as present, ExecutionPhase, WorkflowPresentation};
use super::*;
use crate::session_state::checkpoint::{RestoreMode, RestorePreview};
use anyhow::Context;
use serde_json::json;

mod workbench;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WorkflowPanel {
    Changes,
    Timeline,
    Sessions,
    Attachments,
    Usage,
    Actions,
    Search,
    Recovery,
    SideQuestion,
    Checks,
    Tasks,
    Files,
    Attention,
    Permissions,
}

#[derive(Clone, Debug)]
enum Action {
    None,
    Attention(String, super::attention::Target),
    AttachReference(String),
    Task { owner: SessionId, id: String },
    Revoke(crate::sandbox_approval::ApprovalScope),
    Command(String),
    File(String),
    Checkpoint(String),
    Session(SessionId),
    Paste(usize),
    Image(usize),
    Transcript(usize),
    Continue,
    Model,
    SignIn,
    Inspect,
}

#[derive(Clone, Debug)]
struct Row {
    label: String,
    detail: String,
    action: Action,
}

enum Loaded {
    Rows(Vec<Row>),
    FilePreview(String, String),
    TaskDetail(SessionId, String, String),
    TaskMessage(SessionId, String, String),
    SessionChanged(String),
    SideAnswer(String),
    ReviewBase(String),
    Review(ReviewSnapshot, String, String),
    Diff(String, String),
    Sessions(serde_json::Value),
    Preview(PreviewLease),
    Attachment(String, String),
}

// The token and its recovery ref belong to the view or pending load. Dropping
// either must cancel even when a blocking prepare finishes after cancellation.
struct PreviewLease {
    preview: RestorePreview,
    diff: String,
    manager: std::sync::Arc<crate::session_state::checkpoint::CheckpointManager>,
}

impl std::ops::Deref for PreviewLease {
    type Target = RestorePreview;
    fn deref(&self) -> &Self::Target {
        &self.preview
    }
}

impl Drop for PreviewLease {
    fn drop(&mut self) {
        self.manager.cancel_preview(&self.preview.token);
    }
}

pub(super) struct Workflow {
    pub panel: Option<WorkflowPanel>,
    pub focused: bool,
    pub scope: ReviewScope,
    pub base: String,
    pub(super) query: SingleLineInputState,
    command_input: Option<String>,
    rows: Vec<Row>,
    selected: usize,
    snapshot: Option<ReviewSnapshot>,
    diff_path: String,
    diff: String,
    hunk: usize,
    pending: Option<tokio::task::JoinHandle<anyhow::Result<Loaded>>>,
    refreshed: Instant,
    search_due: Option<Instant>,
    pub scroll: ScrollState,
    pub scroll_metrics: (usize, usize),
    restore_mode: RestoreMode,
    preview: Option<PreviewLease>,
    error: Option<String>,
    pub failure: Option<String>,
    pub phase: ExecutionPhase,
    pub editor_requested: bool,
    pub image_preview: Option<tuika::term::image::ImageData>,
    pub image_layer: tuika::term::image::ImageLayer,
    pub list_viewport: tuika::components::SelectViewportState,
    pub list_area: Rect,
    pub list_start: usize,
    side_question: String,
    side_answer: String,
    inspected_task: Option<(SessionId, String)>,
    inspected_file: String,
    session_archive: u8,
    editing_session: Option<SessionId>,
    session_grouped: bool,
    pub(super) notice: String,
    views: std::collections::HashMap<WorkflowPanel, ViewMemory>,
    restore_selection: Option<String>,
    task_filter: String,
    message_reply: Option<String>,
    message_context: String,
    task_paused: bool,
    pub task_follow: bool,
    pub actionable: bool,
}

#[derive(Clone)]
struct ViewMemory {
    query: String,
    selected: usize,
    key: Option<String>,
    action: Option<Action>,
    offset: usize,
}

fn row_key(row: &Row) -> String {
    match &row.action {
        Action::None => row.label.clone(),
        action => format!("{action:?}"),
    }
}

impl Default for Workflow {
    fn default() -> Self {
        Self {
            panel: None,
            focused: false,
            scope: ReviewScope::WorkingTree,
            base: "origin/main".into(),
            query: SingleLineInputState::new(),
            command_input: None,
            rows: Vec::new(),
            selected: 0,
            snapshot: None,
            diff_path: String::new(),
            diff: String::new(),
            hunk: 0,
            pending: None,
            refreshed: Instant::now(),
            search_due: None,
            scroll: {
                let mut scroll = ScrollState::new();
                scroll.set_offset(0);
                scroll
            },
            scroll_metrics: (0, 0),
            restore_mode: RestoreMode::Conversation,
            preview: None,
            error: None,
            failure: None,
            phase: ExecutionPhase::Thinking,
            editor_requested: false,
            image_preview: None,
            image_layer: tuika::term::image::ImageLayer::new(),
            list_viewport: tuika::components::SelectViewportState::new(),
            list_area: Rect::ZERO,
            list_start: 0,
            side_question: String::new(),
            side_answer: String::new(),
            inspected_task: None,
            inspected_file: String::new(),
            session_archive: 0,
            editing_session: None,
            session_grouped: false,
            notice: String::new(),
            views: Default::default(),
            restore_selection: None,
            task_filter: String::new(),
            message_reply: None,
            message_context: String::new(),
            task_paused: false,
            task_follow: false,
            actionable: true,
        }
    }
}

impl Workflow {
    fn cancel_load(&mut self) {
        if let Some(task) = self.pending.take() {
            task.abort();
        }
    }

    fn visible(&self) -> Vec<usize> {
        if self.command_input.is_some() {
            return Vec::new();
        }
        let query = self.query.text().to_lowercase();
        if self.panel == Some(WorkflowPanel::Files) {
            return workbench::matching_files(&self.rows, &query);
        }
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.label.to_lowercase().contains(&query)
                    || row.detail.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn replace_rows(&mut self, rows: Vec<Row>) {
        let key = self
            .restore_selection
            .take()
            .or_else(|| self.selected_row().map(row_key));
        self.rows = rows;
        let visible = self.visible();
        self.selected = key
            .and_then(|key| {
                visible
                    .iter()
                    .position(|index| row_key(&self.rows[*index]) == key)
            })
            .unwrap_or(self.selected.min(visible.len().saturating_sub(1)));
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(*self.visible().get(self.selected)?)
    }

    pub fn docked(&self, width: u16) -> bool {
        self.panel == Some(WorkflowPanel::Changes) && width >= 110
    }

    pub fn close(&mut self) {
        if let Some(panel) = self.panel
            && panel != WorkflowPanel::Actions
            && self.command_input.is_none()
            && self.preview.is_none()
        {
            self.views.insert(
                panel,
                ViewMemory {
                    query: self.query.text().into(),
                    selected: self.selected,
                    key: self.selected_row().map(row_key),
                    action: self.selected_row().map(|row| row.action.clone()),
                    offset: self.scroll.offset(),
                },
            );
        }
        self.cancel_load();
        self.panel = None;
        self.focused = false;
        self.preview = None;
        self.command_input = None;
        self.image_preview = None;
    }
}

impl App {
    pub(super) fn close_workflow(&mut self) {
        self.workflow.close();
    }

    pub(super) fn set_review_base(&mut self, reference: String) {
        let reference = reference.trim().to_string();
        if reference.is_empty() || matches!(reference.as_str(), "-h" | "--help") {
            let usage =
                "Usage: /diff-base <reference>, for example /diff-base origin/main".to_string();
            self.push_system(usage.clone());
            if self.workflow.panel.is_some() {
                self.workflow.error = Some(usage);
            }
            return;
        }
        if self.pending_ask.is_some()
            || self.pending_sandbox_approval.is_some()
            || self.setup.is_some()
        {
            self.push_system("Finish the open dialog before changing the comparison base.".into());
            return;
        }
        self.open_workflow(WorkflowPanel::Changes);
        self.workflow.cancel_load();
        let root = self.worktree.active_root();
        self.workflow.pending = Some(tokio::spawn(async move {
            super::review::resolve(&root, &format!("{reference}^{{commit}}"))
                .await
                .context("Invalid comparison reference")?;
            Ok(Loaded::ReviewBase(reference))
        }));
    }

    pub(super) async fn handle_workflow_mouse(&mut self, mouse: MouseEvent, area: Rect) -> bool {
        if self.pending_ask.is_some() || self.setup.is_some() {
            return false;
        }
        if self.pending_sandbox_approval.is_some() {
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) {
                self.handle_sandbox_approval_key(KeyEvent::new(
                    if mouse.kind == MouseEventKind::ScrollUp {
                        KeyCode::PageUp
                    } else {
                        KeyCode::PageDown
                    },
                    KeyModifiers::NONE,
                ));
            }
            return true;
        }
        let Some(panel) = super::workflow_render::panel_area(self, area) else {
            return false;
        };
        let inside = mouse.column >= panel.x
            && mouse.column < panel.right()
            && mouse.row >= panel.y
            && mouse.row < panel.bottom();
        if !inside {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.workflow.focused = false;
            }
            return false;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let (content, viewport) = self.workflow.scroll_metrics;
                let offset = self.workflow.scroll.offset();
                self.workflow
                    .scroll
                    .set_offset(if mouse.kind == MouseEventKind::ScrollUp {
                        offset.saturating_sub(3)
                    } else {
                        offset
                            .saturating_add(3)
                            .min(ScrollState::max_offset(content, viewport))
                    });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.workflow.focused = true;
                let list = self.workflow.list_area;
                if mouse.row >= list.y && mouse.row < list.bottom() {
                    self.workflow.selected = (self.workflow.list_start
                        + usize::from(mouse.row - list.y))
                    .min(self.workflow.visible().len().saturating_sub(1));
                    self.workflow.scroll.set_offset(0);
                    if self.workflow.panel == Some(WorkflowPanel::Changes) {
                        self.load_selected_diff();
                    }
                }
            }
            _ => {}
        }
        true
    }

    pub(crate) fn take_editor_request(&mut self) -> bool {
        if !std::mem::take(&mut self.workflow.editor_requested) {
            return false;
        }
        self.should_quit = false;
        true
    }

    pub(crate) async fn edit_prompt(&mut self) {
        let original = self.input_text();
        match tokio::task::spawn_blocking(move || edit_in_external_editor(&original)).await {
            Ok(Ok(text))=>self.set_input_text(text),
            Ok(Err(error))=>self.push_system(format!("Prompt editor failed: {error:#}. Draft preserved. Set VISUAL or EDITOR to an editor command.")),
            Err(error)=>self.push_system(format!("Prompt editor failed: {error}. Draft preserved.")),
        }
    }

    pub(super) fn open_workflow(&mut self, panel: WorkflowPanel) {
        if self.pending_ask.is_some()
            || self.pending_sandbox_approval.is_some()
            || self.setup.is_some()
        {
            self.push_system("Finish the open dialog before opening another view.".into());
            return;
        }
        self.close_workflow();
        self.workflow.panel = Some(panel);
        self.workflow.focused = true;
        self.workflow.query.clear();
        self.workflow.selected = 0;
        self.workflow.scroll.set_offset(0);
        self.workflow.preview = None;
        self.workflow.error = None;
        self.workflow.notice.clear();
        self.workflow.image_preview = None;
        self.workflow.rows.clear();
        if let Some(memory) = self.workflow.views.get(&panel).cloned() {
            self.workflow.query.insert_str(&memory.query);
            self.workflow.selected = memory.selected;
            self.workflow.restore_selection = memory.key;
            self.workflow.scroll.set_offset(memory.offset);
        } else {
            self.workflow.restore_selection = None;
        }
        if panel == WorkflowPanel::Changes {
            self.workflow.snapshot = None;
            self.workflow.diff.clear();
            self.workflow.diff_path.clear();
            self.workflow.hunk = 0;
        }
        match panel {
            WorkflowPanel::Changes => self.load_review(),
            WorkflowPanel::Sessions => self.load_sessions(),
            WorkflowPanel::Timeline => {
                self.workflow.rows = self.session.handles().checkpoints.list().into_iter().map(|checkpoint| Row {
                    label: format!("{} UTC · {} · {} · {}", checkpoint.created_at.format("%m-%d %H:%M"), match checkpoint.success {Some(true)=>"completed",Some(false)=>"failed",None=>"incomplete"},if checkpoint.workspace_available {"chat + files"} else {"chat only"}, checkpoint.prompt),
                    detail: format!("{}\n{}\n{}", checkpoint.id, checkpoint.prompt, checkpoint.workspace_error.unwrap_or_else(|| "Choose conversation, files, or both. A preview is required before restoration.".into())),
                    action: Action::Checkpoint(checkpoint.id),
                }).collect();
            }
            WorkflowPanel::Attachments => self.refresh_attachments(),
            WorkflowPanel::Actions => {
                let hints = self.keymap.hints();
                self.workflow.rows = self
                    .startup
                    .capability_commands
                    .iter()
                    .map(|command| {
                        let action = match command.name.as_str() {
                            "actions" => Some(GlobalAction::Actions),
                            "find" => Some(GlobalAction::Find),
                            "diff" => Some(GlobalAction::Changes),
                            _ => None,
                        };
                        let shortcut = action
                            .and_then(|action| hints.iter().find(|hint| hint.command == action))
                            .map(|hint| format!(" [{}]", hint.keys))
                            .unwrap_or_default();
                        Row {
                            label: format!("/{}  {}{shortcut}", command.name, command.description),
                            detail: format!(
                                "{}\n{}",
                                command.description,
                                command
                                    .args
                                    .iter()
                                    .map(|arg| format!("{}: {}", arg.name, arg.description))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            ),
                            action: Action::Command(command.name.clone()),
                        }
                    })
                    .collect();
                self.workflow.rows.sort_by(|a, b| a.label.cmp(&b.label));
            }
            WorkflowPanel::Search => {
                self.workflow.rows = self
                    .lines
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(index, line)| Row {
                        label: format!("{} · {}", line.author.label(), first_line(&line.text, 140)),
                        detail: line.text.clone(),
                        action: Action::Transcript(index),
                    })
                    .collect();
            }
            WorkflowPanel::Recovery => {
                self.workflow.rows = vec![
                    Row {
                        label: "Continue task after inspecting current state".into(),
                        detail: "Starts a continuation without replaying the last command.".into(),
                        action: Action::Continue,
                    },
                    Row {
                        label: "Change model".into(),
                        detail: "Choose another model, then continue the task.".into(),
                        action: Action::Model,
                    },
                    Row {
                        label: "Sign in / configure provider".into(),
                        detail: "Repair credentials using guided setup.".into(),
                        action: Action::SignIn,
                    },
                    Row {
                        label: "Inspect retained work details".into(),
                        detail: "Reveal the most recent turn's commands and output.".into(),
                        action: Action::Inspect,
                    },
                ];
            }
            WorkflowPanel::Usage => self.load_context(),
            WorkflowPanel::SideQuestion => {}
            WorkflowPanel::Checks
            | WorkflowPanel::Tasks
            | WorkflowPanel::Files
            | WorkflowPanel::Attention
            | WorkflowPanel::Permissions => self.open_workbench(panel),
        }
    }

    fn review_reference(&self) -> anyhow::Result<String> {
        match self.workflow.scope {
            ReviewScope::WorkingTree => Ok("HEAD".into()),
            ReviewScope::Base => Ok(format!("{}^{{commit}}", self.workflow.base)),
            ReviewScope::LastTurn | ReviewScope::Session => self.session.handles().checkpoints.review_tree(self.workflow.scope == ReviewScope::Session).ok_or_else(|| anyhow::anyhow!("No workspace snapshot is available for this scope. Last-turn and session comparisons require a captured Yolop worktree. Choose Working tree or Versus base.")),
        }
    }

    fn load_review(&mut self) {
        self.workflow.cancel_load();
        self.workflow.refreshed = Instant::now();
        let reference = match self.review_reference() {
            Ok(reference) => reference,
            Err(error) => {
                self.workflow.rows.clear();
                self.workflow.snapshot = None;
                self.workflow.diff.clear();
                self.workflow.error = Some(error.to_string());
                return;
            }
        };
        let selected_path = self
            .workflow
            .selected_row()
            .map(|row| row.action.clone())
            .or_else(|| {
                self.workflow
                    .views
                    .get(&WorkflowPanel::Changes)
                    .and_then(|view| view.action.clone())
            })
            .and_then(|action| match action {
                Action::File(path) => Some(path),
                _ => None,
            });
        let root = self.worktree.active_root();
        self.workflow.pending = Some(tokio::spawn(async move {
            let snapshot = super::review::collect(&root, &reference).await?;
            let selected = selected_path
                .as_ref()
                .and_then(|path| snapshot.files.iter().find(|file| &file.path == path))
                .or(snapshot.files.first());
            let (path, diff) = if let Some(file) = selected {
                (
                    file.path.clone(),
                    super::review::file_diff(&root, &snapshot.reference, file)
                        .await
                        .unwrap_or_else(|error| format!("Preview unavailable: {error:#}")),
                )
            } else {
                (String::new(), "No changes in this comparison.".into())
            };
            Ok(Loaded::Review(snapshot, path, diff))
        }));
    }

    fn load_selected_diff(&mut self) {
        let Some(Action::File(path)) = self.workflow.selected_row().map(|row| row.action.clone())
        else {
            return;
        };
        let Some(snapshot) = &self.workflow.snapshot else {
            return;
        };
        let Some(file) = snapshot
            .files
            .iter()
            .find(|file| file.path == path)
            .cloned()
        else {
            return;
        };
        let reference = snapshot.reference.clone();
        let root = self.worktree.active_root();
        self.workflow.cancel_load();
        self.workflow.diff.clear();
        self.workflow.diff_path.clear();
        self.workflow.hunk = 0;
        self.workflow.pending = Some(tokio::spawn(async move {
            Ok(Loaded::Diff(
                path,
                super::review::file_diff(&root, &reference, &file).await?,
            ))
        }));
    }

    fn load_sessions(&mut self) {
        self.workflow.cancel_load();
        self.workflow.search_due = None;
        let root = self
            .session
            .handles()
            .checkpoints
            .sessions_dir()
            .to_path_buf();
        let current = self.session_id();
        let query = self.workflow.query.text().to_string();
        let archive = ["active", "archived", "all"][usize::from(self.workflow.session_archive)];
        self.workflow.pending = Some(tokio::task::spawn_blocking(move || {
            crate::capabilities::sessions::search_sessions(
                &root,
                current,
                &json!({"query":query,"limit":50,"archive":archive}),
            )
            .map(Loaded::Sessions)
            .map_err(anyhow::Error::msg)
        }));
    }

    fn refresh_attachments(&mut self) {
        let raw = self.input_text();
        crate::tui::input::paste_attachment::prune_pending_pastes(&raw, &mut self.pending_pastes);
        self.workflow.rows = self
            .pending_pastes
            .iter()
            .enumerate()
            .map(|(i, (label, text))| Row {
                label: format!("{} · {} bytes", label, text.len()),
                detail: text.clone(),
                action: Action::Paste(i),
            })
            .chain(self.pending_images.iter().enumerate().map(|(i, _)| Row {
                label: format!("Image #{}", i + 1),
                detail: "Attached image. Enter previews it; Delete removes it.".into(),
                action: Action::Image(i),
            }))
            .collect();
    }

    pub(super) fn poll_workflow(&mut self) {
        let finished = self
            .workflow
            .pending
            .as_mut()
            .and_then(FutureExt::now_or_never);
        if let Some(result) = finished {
            self.workflow.pending = None;
            match result {
                Ok(Ok(loaded)) => {
                    self.workflow.error = None;
                    match loaded {
                        Loaded::SessionChanged(message) => { self.workflow.notice = message; self.load_sessions(); }
                        Loaded::Rows(rows) => {
                            self.workflow.replace_rows(rows);
                            if self.workflow.panel == Some(WorkflowPanel::Files) { self.load_file_preview(); }
                        }
                        Loaded::TaskMessage(owner, id, detail) => {
                            self.workflow.notice = detail.lines().next().unwrap_or_default().to_string();
                            self.workflow.task_filter.clear();
                            if let Some(row) = self.workflow.rows.iter_mut().find(|row| matches!(&row.action, Action::Task { owner: row_owner, id: row_id } if owner == *row_owner && id == *row_id)) { row.detail = detail; }
                        }
                        Loaded::TaskDetail(owner, id, detail) => {
                            if let Some(row) = self.workflow.rows.iter_mut().find(|row| matches!(&row.action, Action::Task { owner: row_owner, id: row_id } if owner == *row_owner && id == *row_id)) { row.detail = detail; }
                        }
                        Loaded::FilePreview(path, text) => {
                            self.workflow.inspected_file = path.clone();
                            if let Some(row) = self.workflow.rows.iter_mut().find(|row| matches!(&row.action, Action::AttachReference(reference) if reference == &path)) { row.detail = text; }
                        }
                        Loaded::SideAnswer(answer) => self.workflow.side_answer = answer,
                        Loaded::ReviewBase(reference) => {
                            self.workflow.base = reference;
                            self.workflow.scope = ReviewScope::Base;
                            self.open_workflow(WorkflowPanel::Changes);
                        }
                        Loaded::Review(snapshot, path, diff) => {
                            self.workflow.rows = snapshot
                                .files
                                .iter()
                                .map(|file| Row {
                                    label: format!(
                                        "{}  {}",
                                        super::review::file_display(&file.path),
                                        file.summary
                                    ),
                                    detail: String::new(),
                                    action: Action::File(file.path.clone()),
                                })
                                .collect();
                            self.workflow.snapshot = Some(snapshot);
                            self.workflow.diff_path = path;
                            self.workflow.diff = diff;
                            self.workflow.selected = self.workflow.visible().iter().position(|index| matches!(&self.workflow.rows[*index].action, Action::File(path) if path == &self.workflow.diff_path)).unwrap_or(0);
                        }
                        Loaded::Diff(path, diff) => {
                            self.workflow.diff_path = path;
                            self.workflow.diff = diff;
                        }
                        Loaded::Preview(preview) => {
                            self.workflow.preview = Some(preview);
                            self.workflow.scroll.set_offset(0);
                        }
                        Loaded::Attachment(label, text) => {
                            self.attach_text(&label, &text);
                            self.refresh_attachments();
                        }
                        Loaded::Sessions(mut value) => {
                            if self.workflow.session_grouped && let Some(sessions) = value["sessions"].as_array_mut() {
                                sessions.sort_by_key(|session| (session["pinned"].as_bool() != Some(true), session["workspace"].as_str().unwrap_or_default().to_owned()));
                            }
                            let rows = value["sessions"].as_array().into_iter().flatten().filter_map(|session| {
                                let id = session["session_id"].as_str()?.parse().ok()?;
                                let title = session["title"].as_str().or_else(|| session["snippet"].as_str()).unwrap_or("Untitled session");
                                let workspace = session["workspace"].as_str().unwrap_or("workspace unknown");
                                let branch = session["branch"].as_str().unwrap_or("branch unknown");
                                let state = if session["active_elsewhere"].as_bool() == Some(true) { "active elsewhere" } else if session["failed"].as_bool()==Some(true) {"recorded failure"} else {"saved"};
                                Some(Row {
                                    label:format!("{}{}{} · {} · {} · {}", if session["pinned"].as_bool() == Some(true) {"★ "} else {""}, if session["archived"].as_bool() == Some(true) {"[archived] "} else {""}, first_line(title,80), Path::new(workspace).file_name().and_then(|name| name.to_str()).unwrap_or(workspace), branch,state),
                                    detail:format!("{title}\n{workspace}\nBranch: {branch}\nState: {state}\n\nLast matching message:\n{}\n\nSaved recap:\n{}\n{}\nOpening resumes the existing session. Live sessions retain their exclusive lease.", session["snippet"].as_str().unwrap_or("unknown"),session["recap"].as_str().unwrap_or("No saved recap"), session["ui_warning"].as_str().unwrap_or_default()), action:Action::Session(id),
                                })
                            }).collect();
                            self.workflow.replace_rows(rows);
                        }
                    }
                }
                Ok(Err(error)) => self.workflow.error = Some(format!("{error:#}")),
                Err(error) if !error.is_cancelled() => {
                    self.workflow.error = Some(format!("view update failed: {error}"))
                }
                Err(_) => {}
            }
        }
        if self.workflow.panel == Some(WorkflowPanel::Sessions)
            && self
                .workflow
                .search_due
                .is_some_and(|due| Instant::now() >= due)
        {
            self.load_sessions();
        }
        if self.workflow.panel == Some(WorkflowPanel::Tasks)
            && self.workflow.command_input.is_none()
            && !self.workflow.task_paused
            && self.workflow.pending.is_none()
            && self.workflow.refreshed.elapsed() >= Duration::from_secs(2)
        {
            self.open_workbench(WorkflowPanel::Tasks);
            if let Some(Action::Task { owner, id }) =
                self.workflow.selected_row().map(|row| row.action.clone())
            {
                self.inspect_task(owner, id);
            }
        }
        if self.workflow.panel == Some(WorkflowPanel::Changes)
            && self.workflow.pending.is_none()
            && self.workflow.refreshed.elapsed() >= Duration::from_secs(2)
        {
            self.load_review();
        }
    }

    pub(super) fn workflow_presentation(&self) -> Option<WorkflowPresentation> {
        let panel = self.workflow.panel?;
        let (title, footer) = match panel {
            WorkflowPanel::Changes => (
                "Changes",
                "Up/Down files · PgUp/PgDn diff · Tab scope · h next hunk · a attach hunk · A attach file · Ctrl+X P composer · Esc close",
            ),
            WorkflowPanel::Timeline => (
                "Rewind timeline",
                "c conversation · w files · b both · Enter preview · r redo · Esc close",
            ),
            WorkflowPanel::Sessions => (
                "Sessions",
                "Type to search · Enter resume · Ctrl+S pin · Alt+R rename · Ctrl+Delete archive · Tab archive filter · Alt+G group projects · Esc close",
            ),
            WorkflowPanel::Attachments => (
                "Attachments",
                "Up/Down select · Enter preview image · Delete remove · Esc composer · /attach path#start-end",
            ),
            WorkflowPanel::Usage => ("Context and usage", "PgUp/PgDn scroll · Esc close"),
            WorkflowPanel::SideQuestion => (
                "Side question",
                "PgUp/PgDn scroll · c copy answer · s attach to draft · Esc close",
            ),
            WorkflowPanel::Checks => (
                "Checks and evidence",
                "Type to filter · Up/Down select · PgUp/PgDn output · Ctrl+R refresh · Esc close",
            ),
            WorkflowPanel::Tasks => (
                "Task inspector",
                "Up/Down task · Enter inspect · PgUp/PgDn output · / find · t pause/live · x cancel · m message · r refresh · Esc close",
            ),
            WorkflowPanel::Files => (
                "Attach workspace file",
                "Type fuzzy path#start-end · Up/Down select · Enter attach · PgUp/PgDn preview · Esc cancel",
            ),
            WorkflowPanel::Attention => (
                "Needs attention",
                "Up/Down select · Enter open · Delete acknowledge · n desktop alerts · Esc close",
            ),
            WorkflowPanel::Permissions => (
                "Effective permissions",
                "Up/Down inspect · Delete revoke selected session grant · Esc close",
            ),
            WorkflowPanel::Actions => (
                "Actions",
                "Type to search · Up/Down select · Enter run · Esc close",
            ),
            WorkflowPanel::Search => (
                "Find in transcript",
                "Type to search · Up/Down select · Enter jump · Esc close",
            ),
            WorkflowPanel::Recovery => {
                ("Recover task", "Up/Down select · Enter choose · Esc close")
            }
        };
        let mut view = WorkflowPresentation {
            title: title.into(),
            focus_hint: if self.workflow.focused {
                "focused".into()
            } else {
                "Ctrl+X P focus".into()
            },
            query: self.workflow.query.text().to_string(),
            footer: footer.into(),
            selected: self.workflow.selected,
            rows: self
                .workflow
                .visible()
                .iter()
                .map(|i| self.workflow.rows[*i].label.clone())
                .collect(),
            loading: self.workflow.pending.is_some(),
            query_label: if matches!(
                panel,
                WorkflowPanel::Actions
                    | WorkflowPanel::Search
                    | WorkflowPanel::Sessions
                    | WorkflowPanel::Files
                    | WorkflowPanel::Checks
            ) {
                "Search".into()
            } else {
                String::new()
            },
            ..Default::default()
        };
        view.subtitle = match panel {
            WorkflowPanel::Changes => format!(
                "{} · base {} · {} · hunk {}/{}",
                self.workflow.scope.label(),
                self.workflow.base,
                self.workflow
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.notice.as_str())
                    .unwrap_or(""),
                self.workflow
                    .hunk
                    .saturating_add(1)
                    .min(diff_hunks(&self.workflow.diff).len()),
                diff_hunks(&self.workflow.diff).len()
            ),
            WorkflowPanel::Timeline => format!(
                "Restore: {:?} · {} checkpoints",
                self.workflow.restore_mode,
                self.workflow.rows.len()
            ),
            WorkflowPanel::Actions if self.busy => {
                "Local views stay available. Runtime commands wait until the active turn settles."
                    .into()
            }
            WorkflowPanel::Sessions => format!(
                "{} sessions · {}",
                ["Active", "Archived", "All"][usize::from(self.workflow.session_archive)],
                view.rows.len()
            ),
            _ => format!("{} results", view.rows.len()),
        };
        view.detail = if let Some(command) = self.workflow.command_input.as_ref() {
            view.title = match command.as_str() {
                "rename-session" => "Rename session".into(),
                "task-message" => "Message agent".into(),
                "task-find" => "Find in task output".into(),
                "btw" => "Ask a side question".into(),
                _ => format!("Arguments: /{command}"),
            };
            view.query_label = match command.as_str() {
                "rename-session" => "Name",
                "task-message" | "btw" => "Message",
                "task-find" => "Find",
                _ => "Arguments",
            }
            .into();
            view.footer = "Type text · Enter submit · Esc cancel".into();
            self.startup
                .capability_commands
                .iter()
                .find(|item| &item.name == command)
                .map(|item| {
                    item.args
                        .iter()
                        .map(|arg| format!("{}: {}", arg.name, arg.description))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| match command.as_str() {
                    "rename-session" => "Enter a short session name. Enter saves; Esc returns without renaming.".into(),
                    "task-message" => format!("{}\n\nEnter records and attempts delivery to this agent. Delivery status is shown separately; Esc cancels.",self.workflow.message_context),
                    "btw" => "Ask about the current conversation. The answer uses existing context without tools or changes to history.".into(),
                    _ => String::new(),
                })
        } else if let Some(preview) = &self.workflow.preview {
            view.title = "Confirm restore".into();
            view.footer = "Enter confirm this preview · Esc return without restoring".into();
            format!(
                "Mode: {:?}\nBefore: {}\n\nWorkspace: {} captured path(s) will change\n{}\n\nOnly captured workspace files are restored. Git commits, external services, and ignored files are unchanged.\n\nRestoration diff, current snapshot to target:\n{}",
                preview.mode,
                preview
                    .prompt
                    .as_deref()
                    .unwrap_or("the most recently abandoned branch"),
                preview.changed_paths.len(),
                preview.changed_paths.join("\n"),
                preview.diff,
            )
        } else if panel == WorkflowPanel::Usage {
            self.workflow
                .selected_row()
                .map(|row| row.detail.clone())
                .unwrap_or_else(|| present::usage_detail(&self.presentation_state()))
        } else if panel == WorkflowPanel::SideQuestion {
            format!(
                "{}\n\n{}\n\nSide questions use current context, have no tools, and stay outside conversation history.",
                self.workflow.side_question,
                if self.workflow.side_answer.is_empty() {
                    if self.workflow.pending.is_some() {
                        "Asking…"
                    } else {
                        "No answer was returned. Press r to retry, or close and ask a new /btw question."
                    }
                } else {
                    &self.workflow.side_answer
                }
            )
        } else if panel == WorkflowPanel::Changes {
            self.workflow.diff.clone()
        } else if panel == WorkflowPanel::Recovery {
            present::recovery_detail(
                self.workflow
                    .failure
                    .as_deref()
                    .unwrap_or("No recorded turn failure."),
            )
        } else {
            self.workflow
                .selected_row()
                .map(|row| row.detail.clone())
                .unwrap_or_else(|| "No matching entries.".into())
        };
        if panel == WorkflowPanel::Tasks
            && self.workflow.command_input.is_none()
            && !self.workflow.task_filter.is_empty()
        {
            let needle = self.workflow.task_filter.to_lowercase();
            let matches = view
                .detail
                .lines()
                .enumerate()
                .filter(|(_, line)| line.to_lowercase().contains(&needle))
                .map(|(line, text)| format!("{}: {text}", line + 1))
                .collect::<Vec<_>>();
            view.detail = format!(
                "Output filter: {} · {} matching lines. / changes or clears filter.\n\n{}",
                self.workflow.task_filter,
                matches.len(),
                matches.join("\n")
            );
        }
        if !self.workflow.notice.is_empty() {
            view.subtitle
                .push_str(&format!(" · {}", self.workflow.notice));
        }
        if let Some(error) = &self.workflow.error {
            view.detail = format!("{error}\n\n{}", view.detail);
        }
        // Saved titles, tool labels and paths are data, not terminal commands.
        view.rows
            .iter_mut()
            .for_each(|row| *row = super::review::file_display(row));
        Some(view)
    }

    fn attach_text(&mut self, label: &str, text: &str) {
        let mut index = 1;
        let placeholder = loop {
            let candidate = format!("[{label} #{index}]");
            if !self.pending_pastes.iter().any(|(key, _)| key == &candidate) {
                break candidate;
            }
            index += 1;
        };
        self.composer_insert_str(&format!(" {placeholder} "));
        self.pending_pastes
            .push((placeholder, format!("{label}\n{text}")));
    }

    pub(super) fn attach_file(&mut self, reference: String) {
        self.open_workflow(WorkflowPanel::Attachments);
        let root = self.worktree.active_root();
        self.workflow.pending = Some(tokio::task::spawn_blocking(move || {
            let (label, text) = super::attachment::text(&root, &reference)?;
            Ok(Loaded::Attachment(label, text))
        }));
    }

    fn prepare_visual_restore(&mut self, redo: bool) {
        if self.busy {
            self.workflow.error = Some(
                "Wait for the active turn and workspace writers to finish before restoring.".into(),
            );
            return;
        }
        let selected = self.workflow.selected_row().map(|row| row.action.clone());
        let manager = self.session.handles().checkpoints.clone();
        let mode = self.workflow.restore_mode;
        self.workflow.cancel_load();
        self.workflow.pending = Some(tokio::spawn(async move {
            let mut lease = tokio::task::spawn_blocking(move || -> anyhow::Result<PreviewLease> {
                let preview = if redo {
                    manager.prepare_redo(mode)?
                } else if let Some(Action::Checkpoint(id)) = selected {
                    manager.prepare_rewind(&id, mode)?
                } else {
                    anyhow::bail!("Select a checkpoint first")
                };
                Ok(PreviewLease {
                    preview,
                    manager,
                    diff: String::new(),
                })
            })
            .await??;
            lease.diff =
                match lease.manager.preview_comparison(&lease.token)? {
                    Some(comparison) => {
                        match super::review::git(
                            &comparison.root,
                            &[
                                "diff",
                                "--no-ext-diff",
                                "--no-textconv",
                                "--relative",
                                "--no-renames",
                                &comparison.before,
                                &comparison.after,
                                "--",
                            ],
                        )
                        .await
                        {
                            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                            Err(error) => return Err(error.context(
                                "Restore preview cancelled because its diff could not be loaded",
                            )),
                        }
                    }
                    None => "Conversation-only restore: workspace files stay as they are.".into(),
                };
            Ok(Loaded::Preview(lease))
        }));
    }

    pub(super) async fn handle_workflow_key(&mut self, key: KeyEvent) {
        if !self.workflow.actionable && key.code == KeyCode::Enter {
            self.workflow.error =
                Some("Resize the terminal to review this action before continuing.".into());
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.close_workflow();
            return;
        }
        if let Some(command) = self.workflow.command_input.clone() {
            match key.code {
                KeyCode::Esc => {
                    self.workflow.command_input = None;
                    self.workflow.query.clear();
                    self.workflow.error = None;
                }
                KeyCode::Enter => {
                    let arguments = self.workflow.query.text().trim().to_string();
                    if command == "task-find" {
                        self.workflow.task_filter = arguments;
                        self.workflow.command_input = None;
                        self.workflow.query.clear();
                        self.workflow.scroll.jump_to_top();
                        self.workflow.task_follow = false;
                        return;
                    }
                    if arguments.is_empty() {
                        self.workflow.error = Some("An argument is required.".into());
                        return;
                    }
                    if command == "task-message" {
                        self.send_task_message(arguments);
                        return;
                    }
                    if command == "rename-session" {
                        self.update_session_labels(Some(arguments));
                        return;
                    }
                    self.close_workflow();
                    Box::pin(self.handle_command(&format!("{command} {arguments}"))).await;
                }
                KeyCode::Backspace => {
                    self.edit_workflow_query(key);
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.workflow.query.text().len() < 4096 =>
                {
                    self.workflow.query.insert_char(ch);
                }
                _ => self.edit_workflow_query(key),
            }
            return;
        }
        if let Some(action) = self.dispatch_global_key(key)
            && matches!(
                action,
                GlobalAction::Actions
                    | GlobalAction::Prefix
                    | GlobalAction::Changes
                    | GlobalAction::Find
                    | GlobalAction::FocusWorkflow
                    | GlobalAction::Editor
                    | GlobalAction::Quit
            )
        {
            self.run_global_action(action);
            return;
        }
        if key.code == KeyCode::Esc {
            if self.workflow.preview.is_some() {
                self.workflow.preview = None;
            } else {
                self.close_workflow();
            }
            return;
        }
        if let Some(preview) = self.workflow.preview.as_ref() {
            if matches!(
                key.code,
                KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home | KeyCode::End
            ) {
                self.scroll_workflow_detail(key.code);
                return;
            }
            if key.code == KeyCode::Enter && key.modifiers.is_empty() {
                let token = preview.token.clone();
                match self.session.handles().checkpoints.confirm(&token).await {
                    Ok(message) => {
                        self.close_workflow();
                        self.refresh_after_checkpoint_restore(message).await;
                    }
                    Err(error) => {
                        self.workflow.preview = None;
                        self.workflow.error = Some(format!("Restore refused: {error:#}"));
                    }
                }
            }
            return;
        }
        let panel = self.workflow.panel;
        match key.code {
            KeyCode::Home | KeyCode::End
                if !matches!(
                    panel,
                    Some(
                        WorkflowPanel::Actions
                            | WorkflowPanel::Search
                            | WorkflowPanel::Sessions
                            | WorkflowPanel::Files
                            | WorkflowPanel::Checks
                    )
                ) =>
            {
                self.scroll_workflow_detail(key.code)
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                self.workflow.task_follow = false;
                let amount = self.workflow.scroll_metrics.1.max(1);
                let offset = self.workflow.scroll.offset();
                self.workflow
                    .scroll
                    .set_offset(if key.code == KeyCode::PageUp {
                        offset.saturating_sub(amount)
                    } else {
                        offset.saturating_add(amount).min(ScrollState::max_offset(
                            self.workflow.scroll_metrics.0,
                            self.workflow.scroll_metrics.1,
                        ))
                    });
            }
            KeyCode::Up | KeyCode::Down => {
                let count = self.workflow.visible().len();
                self.workflow.selected = if key.code == KeyCode::Up {
                    self.workflow.selected.saturating_sub(1)
                } else {
                    self.workflow
                        .selected
                        .saturating_add(1)
                        .min(count.saturating_sub(1))
                };
                self.workflow.scroll.set_offset(0);
                self.workflow.image_preview = None;
                if panel == Some(WorkflowPanel::Changes) {
                    self.load_selected_diff();
                } else if panel == Some(WorkflowPanel::Files) {
                    self.load_file_preview();
                }
            }
            KeyCode::Char('r')
                if panel == Some(WorkflowPanel::Checks)
                    && key.modifiers == KeyModifiers::CONTROL
                    || matches!(
                        panel,
                        Some(
                            WorkflowPanel::Tasks
                                | WorkflowPanel::Permissions
                                | WorkflowPanel::Attention
                        )
                    ) && key.modifiers.is_empty() =>
            {
                self.open_workbench(panel.unwrap());
            }
            KeyCode::Char('r')
                if panel == Some(WorkflowPanel::SideQuestion)
                    && self.workflow.pending.is_none() =>
            {
                let question = self.workflow.side_question.clone();
                self.ask_side_question(&question);
            }
            KeyCode::Char('x') if panel == Some(WorkflowPanel::Tasks) => {
                if let Some(Action::Task { owner, id }) =
                    self.workflow.selected_row().map(|row| row.action.clone())
                    && let Some(index) = self
                        .session_tasks
                        .rows
                        .iter()
                        .position(|row| row.task.session_id == owner && row.task.id == id)
                {
                    self.background_selected = index;
                    self.cancel_selected_task().await;
                    self.workflow.notice = self
                        .lines
                        .last()
                        .map(|line| line.text.clone())
                        .unwrap_or_default();
                    self.open_workbench(WorkflowPanel::Tasks);
                }
            }
            KeyCode::Char('m') if panel == Some(WorkflowPanel::Tasks) => self.begin_task_message(),
            KeyCode::Char('/') if panel == Some(WorkflowPanel::Tasks) => {
                self.workflow.command_input = Some("task-find".into());
                self.workflow.query.clear();
            }
            KeyCode::Char('t') if panel == Some(WorkflowPanel::Tasks) => {
                self.workflow.task_paused = !self.workflow.task_paused;
                self.workflow.notice = if self.workflow.task_paused {
                    "Live output paused"
                } else {
                    "Live output refreshes every 2 seconds"
                }
                .into();
            }
            KeyCode::Tab if panel == Some(WorkflowPanel::Changes) => {
                self.workflow.scope = self.workflow.scope.next();
                self.workflow.query.clear();
                self.workflow.selected = 0;
                self.workflow.scroll.set_offset(0);
                self.workflow.rows.clear();
                self.workflow.snapshot = None;
                self.workflow.diff.clear();
                self.workflow.diff_path.clear();
                self.workflow.hunk = 0;
                self.load_review();
            }
            KeyCode::Tab if panel == Some(WorkflowPanel::Sessions) => {
                self.workflow.session_archive = (self.workflow.session_archive + 1) % 3;
                self.load_sessions();
            }
            KeyCode::Char('s')
                if panel == Some(WorkflowPanel::Sessions)
                    && key.modifiers == KeyModifiers::CONTROL =>
            {
                self.toggle_session_label(false)
            }
            KeyCode::Delete
                if panel == Some(WorkflowPanel::Sessions)
                    && key.modifiers == KeyModifiers::CONTROL =>
            {
                self.toggle_session_label(true)
            }
            KeyCode::Char('r')
                if panel == Some(WorkflowPanel::Sessions) && key.modifiers == KeyModifiers::ALT =>
            {
                if let Some(Action::Session(id)) =
                    self.workflow.selected_row().map(|row| row.action.clone())
                {
                    self.workflow.editing_session = Some(id);
                    self.workflow.command_input = Some("rename-session".into());
                    self.workflow.query.clear();
                }
            }
            KeyCode::Char('g')
                if panel == Some(WorkflowPanel::Sessions) && key.modifiers == KeyModifiers::ALT =>
            {
                self.workflow.session_grouped = !self.workflow.session_grouped;
                self.load_sessions();
            }
            KeyCode::Char('c' | 'w' | 'b') if panel == Some(WorkflowPanel::Timeline) => {
                self.workflow.restore_mode = match key.code {
                    KeyCode::Char('w') => RestoreMode::Workspace,
                    KeyCode::Char('b') => RestoreMode::Both,
                    _ => RestoreMode::Conversation,
                };
            }
            KeyCode::Char('r') if panel == Some(WorkflowPanel::Timeline) => {
                self.prepare_visual_restore(true)
            }
            KeyCode::Char('c') if panel == Some(WorkflowPanel::SideQuestion) => {
                if let Err(error) = tuika::term::clipboard::write(
                    &mut std::io::stdout(),
                    &self.workflow.side_answer,
                ) {
                    self.workflow.error = Some(format!("Copy failed: {error}"));
                } else {
                    self.workflow.notice = "Copy requested through the terminal clipboard. Terminal support is required.".into();
                }
            }
            KeyCode::Char('s') if panel == Some(WorkflowPanel::SideQuestion) => {
                if !self.workflow.side_answer.is_empty() {
                    let text = format!(
                        "Question: {}\nAnswer: {}",
                        self.workflow.side_question, self.workflow.side_answer
                    );
                    self.attach_text("Side question", &text);
                    self.workflow.focused = false;
                }
            }
            KeyCode::Char('h') if panel == Some(WorkflowPanel::Changes) => {
                let hunks = diff_hunks(&self.workflow.diff);
                if !hunks.is_empty() {
                    self.workflow.hunk = (self.workflow.hunk + 1) % hunks.len();
                    self.workflow.scroll.set_offset(hunks[self.workflow.hunk].0);
                }
            }
            KeyCode::Char('a' | 'A') if panel == Some(WorkflowPanel::Changes) => {
                if let Some(Action::File(path)) =
                    self.workflow.selected_row().map(|row| row.action.clone())
                    && path == self.workflow.diff_path
                    && !self.workflow.diff.is_empty()
                {
                    let text = if key.code == KeyCode::Char('a') {
                        diff_hunks(&self.workflow.diff)
                            .get(self.workflow.hunk)
                            .map(|(_, text)| text.clone())
                            .unwrap_or_else(|| self.workflow.diff.clone())
                    } else {
                        self.workflow.diff.clone()
                    };
                    self.attach_text(
                        &format!("Diff {}", super::review::file_display(&path)),
                        &text,
                    );
                    self.workflow.focused = false;
                }
            }
            KeyCode::Delete | KeyCode::Backspace if panel == Some(WorkflowPanel::Attachments) => {
                match self.workflow.selected_row().map(|row| row.action.clone()) {
                    Some(Action::Paste(index)) => {
                        let (key, _) = self.pending_pastes.remove(index);
                        self.set_input_text(self.input_text().replace(&key, ""));
                    }
                    Some(Action::Image(index)) => {
                        self.pending_images.remove(index);
                    }
                    _ => {}
                }
                self.workflow.image_preview = None;
                self.refresh_attachments();
                self.workflow.selected = self
                    .workflow
                    .selected
                    .min(self.workflow.rows.len().saturating_sub(1));
            }
            KeyCode::Char('n') if panel == Some(WorkflowPanel::Attention) => {
                self.attention.protocol = (self.attention.protocol + 1) % 3;
                self.workflow.notice = format!(
                    "Desktop alerts for this process: {}. Only new events while focus is known to be elsewhere notify. Terminal support is required.",
                    ["Off", "OSC 9", "OSC 777"][usize::from(self.attention.protocol)]
                );
                self.set_attention_protocol();
            }
            KeyCode::Delete if panel == Some(WorkflowPanel::Attention) => {
                if let Some(Action::Attention(key, _)) =
                    self.workflow.selected_row().map(|row| row.action.clone())
                {
                    self.attention.acknowledge(&key);
                    self.open_workbench(WorkflowPanel::Attention);
                }
            }
            KeyCode::Delete if panel == Some(WorkflowPanel::Permissions) => {
                if let Some(Action::Revoke(scope)) =
                    self.workflow.selected_row().map(|row| row.action.clone())
                {
                    self.session.handles().sandbox_approval_gate.revoke(scope);
                    self.open_workbench(WorkflowPanel::Permissions);
                }
            }
            KeyCode::Enter => self.activate_workflow_row().await,
            KeyCode::Backspace => {
                self.edit_workflow_query(key);
                self.workflow.selected = 0;
                self.workflow_query_changed();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && self.workflow.query.text().len() < 4096
                    && matches!(
                        panel,
                        Some(
                            WorkflowPanel::Actions
                                | WorkflowPanel::Search
                                | WorkflowPanel::Sessions
                                | WorkflowPanel::Files
                                | WorkflowPanel::Checks
                        )
                    ) =>
            {
                self.workflow.query.insert_char(ch);
                self.workflow.selected = 0;
                self.workflow_query_changed();
            }
            _ if matches!(
                panel,
                Some(
                    WorkflowPanel::Actions
                        | WorkflowPanel::Search
                        | WorkflowPanel::Sessions
                        | WorkflowPanel::Files
                        | WorkflowPanel::Checks
                )
            ) =>
            {
                self.edit_workflow_query(key)
            }
            _ => {}
        }
    }

    fn edit_workflow_query(&mut self, key: KeyEvent) {
        if let Some(event) = tuika::translate_event(CrosstermEvent::Key(key)) {
            if self.workflow.query.text().len() >= 4096
                && matches!(key.code, KeyCode::Char(_))
                && key.modifiers.is_empty()
            {
                return;
            }
            if self.workflow.query.handle(&event) == InputOutcome::Changed
                && self.workflow.command_input.is_none()
            {
                self.workflow.selected = 0;
                self.workflow_query_changed();
            }
        }
    }

    fn scroll_workflow_detail(&mut self, key: KeyCode) {
        self.workflow.task_follow = key == KeyCode::End;
        let (content, viewport) = self.workflow.scroll_metrics;
        let offset = self.workflow.scroll.offset();
        self.workflow.scroll.set_offset(match key {
            KeyCode::PageUp => offset.saturating_sub(viewport.max(1)),
            KeyCode::PageDown => offset
                .saturating_add(viewport.max(1))
                .min(ScrollState::max_offset(content, viewport)),
            KeyCode::Home => 0,
            KeyCode::End => ScrollState::max_offset(content, viewport),
            _ => offset,
        });
    }

    fn workflow_query_changed(&mut self) {
        if self.workflow.panel == Some(WorkflowPanel::Sessions) {
            self.workflow.cancel_load();
            self.workflow.search_due = Some(Instant::now() + Duration::from_millis(200));
        }
        if self.workflow.panel == Some(WorkflowPanel::Files) {
            self.load_file_preview();
        }
        self.workflow.scroll.set_offset(0);
    }

    pub(super) fn workflow_paste(&mut self, text: &str) {
        if self.workflow.command_input.is_some()
            || matches!(
                self.workflow.panel,
                Some(
                    WorkflowPanel::Actions
                        | WorkflowPanel::Search
                        | WorkflowPanel::Sessions
                        | WorkflowPanel::Files
                        | WorkflowPanel::Checks
                )
            )
        {
            let text: String = text
                .chars()
                .filter(|ch| !ch.is_control())
                .take(1000)
                .collect();
            let available = 4096usize.saturating_sub(self.workflow.query.text().len());
            let text = &text[..text.floor_char_boundary(available.min(text.len()))];
            self.workflow.query.insert_str(text);
            self.workflow.selected = 0;
            self.workflow_query_changed();
        }
    }

    async fn activate_workflow_row(&mut self) {
        let Some(action) = self.workflow.selected_row().map(|row| row.action.clone()) else {
            return;
        };
        match action {
            Action::None | Action::Revoke(_) => {}
            Action::Attention(key, target) => {
                self.attention.acknowledge(&key);
                match target {
                    super::attention::Target::Task(owner, id) => {
                        self.open_workflow(WorkflowPanel::Tasks);
                        if let Some(selected) = self.workflow.visible().iter().position(|index| matches!(&self.workflow.rows[*index].action, Action::Task { owner: row_owner, id: row_id } if *row_owner == owner && row_id == &id)) { self.workflow.selected = selected; }
                        self.inspect_task(owner, id);
                    }
                    super::attention::Target::Recovery => {
                        self.open_workflow(WorkflowPanel::Recovery)
                    }
                    super::attention::Target::Conversation => {
                        self.close_workflow();
                        self.scroll
                            .jump_to_bottom(self.scroll_metrics.0, self.scroll_metrics.1);
                    }
                }
            }
            Action::AttachReference(path) => {
                let reference = format!(
                    "{}{}",
                    path,
                    workbench::range_suffix(self.workflow.query.text())
                );
                self.attach_file(reference);
            }
            Action::Task { owner, id } => self.inspect_task(owner, id),
            Action::Checkpoint(_) => self.prepare_visual_restore(false),
            Action::Session(id) => {
                if self.busy || !self.queued_messages.is_empty() {
                    self.workflow.error =
                        Some("Finish or cancel the current work before switching sessions.".into());
                    return;
                }
                if let Err(error) = self.save_draft() {
                    self.workflow.error = Some(format!("Could not save draft: {error:#}"));
                    return;
                }
                let target = self
                    .session
                    .handles()
                    .checkpoints
                    .sessions_dir()
                    .join(id.to_string());
                if let Err(error) = crate::runtime::session_log::lock_session(&target) {
                    self.workflow.error = Some(format!("Cannot open session: {error:#}"));
                    return;
                }
                self.open_session = Some(id);
                self.should_quit = true;
            }
            Action::Command(name) => {
                if self.busy && !is_local_view_command(&name) {
                    self.workflow.error=Some("This command is available after the active turn settles. Your draft is preserved.".into());
                    return;
                }
                if self
                    .startup
                    .capability_commands
                    .iter()
                    .any(|cmd| cmd.name == name && cmd.args.iter().any(|arg| arg.required))
                {
                    self.workflow.command_input = Some(name);
                    self.workflow.query.clear();
                    self.workflow.error = None;
                } else {
                    self.close_workflow();
                    Box::pin(self.handle_command(&name)).await;
                }
            }
            Action::Transcript(index) => {
                self.close_workflow();
                self.jump_to_transcript(index);
            }
            Action::Continue => {
                if self.busy {
                    self.workflow.error = Some("The task is already running.".into());
                    return;
                }
                self.close_workflow();
                let prompt="Continue the interrupted task. Inspect current workspace state and completed work before retrying. Do not blindly replay commands or repeat completed external actions.".to_string();
                self.push_user(prompt.clone());
                self.start_turn_with_images(prompt, Vec::new());
            }
            Action::Model => {
                self.close_workflow();
                self.start_model_setup();
            }
            Action::SignIn => {
                self.close_workflow();
                self.start_setup_for_provider(&self.model.provider_name(), true)
                    .await;
            }
            Action::Inspect => {
                self.close_workflow();
                self.toggle_compact_work_details();
            }
            Action::Image(index) => self.preview_image(index),
            Action::File(_) | Action::Paste(_) => {}
        }
    }

    fn jump_to_transcript(&mut self, index: usize) {
        if !self.render_mode.is_fullscreen() {
            self.push_system(
                self.lines
                    .get(index)
                    .map(|line| line.text.clone())
                    .unwrap_or_default(),
            );
            return;
        }
        let width = self.transcript_cache.width.max(1);
        let mut lines = Vec::new();
        let mut links = Vec::new();
        render::append_transcript_range(
            &mut lines,
            &mut links,
            &self.lines[..index.min(self.lines.len())],
            0,
            width,
            None,
        );
        self.scroll.set_offset(lines.len());
    }

    fn preview_image(&mut self, index: usize) {
        use base64::Engine;
        let result = (|| -> anyhow::Result<_> {
            let Some(ContentPart::Image(part)) = self.pending_images.get(index) else {
                anyhow::bail!("Image no longer attached")
            };
            let encoded = part
                .base64
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("Only local attached images can be previewed"))?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(encoded)?;
            let mut reader =
                image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
            let mut limits = image::Limits::default();
            limits.max_alloc = Some(32 * 1024 * 1024);
            reader.limits(limits);
            let image = reader.decode()?.thumbnail(1024, 768).into_rgba8();
            tuika::term::image::ImageData::from_rgba(
                image.width(),
                image.height(),
                image.into_raw(),
            )
            .ok_or_else(|| anyhow::anyhow!("invalid image dimensions"))
        })();
        match result {
            Ok(image) => self.workflow.image_preview = Some(image),
            Err(error) => {
                self.workflow.error = Some(format!("Image preview unavailable: {error:#}"))
            }
        }
    }
}

pub(super) fn is_local_view_command(name: &str) -> bool {
    matches!(
        name,
        "attach"
            | "btw"
            | "diff-base"
            | "diff"
            | "timeline"
            | "sessions"
            | "attachments"
            | "context"
            | "actions"
            | "find"
            | "recover"
            | "checks"
            | "tasks"
            | "files"
            | "attention"
            | "permissions"
    )
}

fn diff_hunks(diff: &str) -> Vec<(usize, String)> {
    let lines = diff.lines().collect::<Vec<_>>();
    let starts = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| line.starts_with("@@ ").then_some(i))
        .collect::<Vec<_>>();
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(lines.len());
            let header = lines[..*starts.first().unwrap_or(&0)].join("\n");
            (
                *start,
                format!("{header}\n{}", lines[*start..end].join("\n")),
            )
        })
        .collect()
}

fn edit_in_external_editor(original: &str) -> anyhow::Result<String> {
    use std::io::{Read, Write};
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .ok_or_else(|| anyhow::anyhow!("VISUAL and EDITOR are unset"))?;
    let mut file = tempfile::Builder::new()
        .prefix("yolop-prompt-")
        .suffix(".txt")
        .tempfile()?;
    file.write_all(original.as_bytes())?;
    file.flush()?;
    #[cfg(unix)]
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("yolop-editor")
        .arg(file.path())
        .status()?;
    #[cfg(not(unix))]
    let status = std::process::Command::new("cmd")
        .args(["/C", &format!("{editor} \"%YOLOP_EDITOR_FILE%\"")])
        .env("YOLOP_EDITOR_FILE", file.path())
        .status()?;
    anyhow::ensure!(status.success(), "editor exited with {status}");
    let read = std::fs::File::open(file.path())?;
    anyhow::ensure!(
        read.metadata()?.is_file(),
        "editor result must be a regular file"
    );
    let mut bytes = Vec::new();
    read.take(super::attachment::MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= super::attachment::MAX_ATTACHMENT_BYTES,
        "edited prompt exceeds 512 KiB"
    );
    Ok(String::from_utf8(bytes)?)
}

#[cfg(test)]
mod preview_lifetime_tests {
    use super::*;

    async fn prepared_app() -> super::super::tests::TestApp {
        let mut test = super::super::tests::app_with_llmsim().await;
        test.app.setup = None;
        test.app.start_turn("keep this completed task".into());
        test.app.pump_turn_until_idle_for_test().await;
        test.app.dispatch_command_for_test("timeline").await;
        test.app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while test.app.workflow.preview.is_none() {
            assert!(Instant::now() < deadline, "preview did not load");
            test.app.poll_workflow();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        test
    }

    #[tokio::test]
    async fn quit_invalidates_the_pending_restore_without_rewinding() {
        let mut test = prepared_app().await;
        let token = test.app.workflow.preview.as_ref().unwrap().token.clone();
        test.app
            .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .await;
        assert!(test.app.should_quit);
        assert!(test.app.workflow.preview.is_none());
        assert!(
            test.app
                .session
                .handles()
                .checkpoints
                .confirm(&token)
                .await
                .is_err()
        );
        assert!(
            test.app
                .session
                .active_lines()
                .await
                .unwrap()
                .iter()
                .any(|line| line.text.contains("keep this completed task"))
        );
    }

    #[tokio::test]
    async fn discarding_a_completed_preview_load_invalidates_its_token() {
        let mut test = prepared_app().await;
        let preview = test.app.workflow.preview.take().unwrap();
        let token = preview.token.clone();
        let task = tokio::spawn(async move { Ok::<_, anyhow::Error>(Loaded::Preview(preview)) });
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        test.app.workflow.pending = Some(task);
        test.app.close_workflow();
        assert!(
            test.app
                .session
                .handles()
                .checkpoints
                .confirm(&token)
                .await
                .is_err()
        );
    }
}

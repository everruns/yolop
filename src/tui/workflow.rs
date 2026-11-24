//! Yolop workflow composition over tuika's existing views and input models.

use super::review::{ReviewScope, ReviewSnapshot};
use super::workflow_presentation::{self as present, ExecutionPhase, WorkflowPresentation};
use super::*;
use crate::session_state::checkpoint::{RestoreMode, RestorePreview};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowPanel {
    Changes,
    Timeline,
    Sessions,
    Attachments,
    Usage,
    Actions,
    Search,
    Recovery,
}

#[derive(Clone, Debug)]
enum Action {
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
    Review(ReviewSnapshot, String, String),
    Diff(String, String),
    Sessions(serde_json::Value),
    Preview(RestorePreview),
    Attachment(String, String),
}

pub(super) struct Workflow {
    pub panel: Option<WorkflowPanel>,
    pub focused: bool,
    pub scope: ReviewScope,
    pub base: String,
    query: String,
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
    preview: Option<RestorePreview>,
    error: Option<String>,
    pub failure: Option<String>,
    pub phase: ExecutionPhase,
    pub editor_requested: bool,
    pub image_preview: Option<tuika::term::image::ImageData>,
    pub image_layer: tuika::term::image::ImageLayer,
    pub list_viewport: tuika::components::SelectViewportState,
    pub list_area: Rect,
    pub list_start: usize,
}

impl Default for Workflow {
    fn default() -> Self {
        Self {
            panel: None,
            focused: false,
            scope: ReviewScope::WorkingTree,
            base: "origin/main".into(),
            query: String::new(),
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
        let query = self.query.to_lowercase();
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

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(*self.visible().get(self.selected)?)
    }

    pub fn docked(&self, width: u16) -> bool {
        self.panel == Some(WorkflowPanel::Changes) && width >= 110
    }

    pub fn close(&mut self) {
        self.cancel_load();
        self.panel = None;
        self.focused = false;
        self.preview = None;
        self.command_input = None;
        self.image_preview = None;
    }
}

impl App {
    fn close_workflow(&mut self) {
        if let Some(preview) = self.workflow.preview.as_ref() {
            self.session
                .handles()
                .checkpoints
                .cancel_preview(&preview.token);
        }
        self.workflow.close();
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
        self.workflow.image_preview = None;
        self.workflow.rows.clear();
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
            WorkflowPanel::Usage => {}
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
        let selected_path = self.workflow.selected_row().and_then(|row| {
            if let Action::File(path) = &row.action {
                Some(path.clone())
            } else {
                None
            }
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
        let query = self.workflow.query.clone();
        self.workflow.pending = Some(tokio::task::spawn_blocking(move || {
            crate::capabilities::sessions::search_sessions(
                &root,
                current,
                &json!({"query":query,"limit":50}),
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
                        Loaded::Sessions(value) => {
                            self.workflow.rows = value["sessions"].as_array().into_iter().flatten().filter_map(|session| {
                                let id = session["session_id"].as_str()?.parse().ok()?;
                                let title = session["title"].as_str().or_else(|| session["snippet"].as_str()).unwrap_or("Untitled session");
                                let workspace = session["workspace"].as_str().unwrap_or("workspace unknown");
                                let branch = session["branch"].as_str().unwrap_or("branch unknown");
                                let state = if session["failed"].as_bool()==Some(true) {"recorded failure"} else {"saved"};
                                Some(Row {
                                    label:format!("{} · {} · {}", first_line(title,80),branch,state),
                                    detail:format!("{title}\n{workspace}\nBranch: {branch}\nState: {state}\n\nLast matching message:\n{}\n\nSaved recap:\n{}\n\nOpening resumes the existing session. Live sessions retain their exclusive lease.", session["snippet"].as_str().unwrap_or("unknown"),session["recap"].as_str().unwrap_or("No saved recap")), action:Action::Session(id),
                                })
                            }).collect();
                            self.workflow.selected = 0;
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
                "Type to search title, repository, branch or messages · Up/Down select · Enter resume · Esc close",
            ),
            WorkflowPanel::Attachments => (
                "Attachments",
                "Up/Down select · Enter preview image · Delete remove · Esc composer · /attach path#start-end",
            ),
            WorkflowPanel::Usage => ("Context and usage", "PgUp/PgDn scroll · Esc close"),
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
            query: self.workflow.query.clone(),
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
                WorkflowPanel::Actions | WorkflowPanel::Search | WorkflowPanel::Sessions
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
            _ => format!("{} results", view.rows.len()),
        };
        view.detail = if let Some(command) = self.workflow.command_input.as_ref() {
            view.title = format!("Arguments: /{command}");
            view.query_label = "Arguments".into();
            view.footer = "Type arguments · Enter run · Esc return to actions".into();
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
                .unwrap_or_default()
        } else if let Some(preview) = &self.workflow.preview {
            view.title = "Confirm restore".into();
            view.footer = "Enter confirm this preview · Esc return without restoring".into();
            format!(
                "Mode: {:?}\nBefore: {}\n\nWorkspace: {} captured path(s) will change\n{}\n\nOnly captured workspace files are restored. Git commits, external services, and ignored files are unchanged.",
                preview.mode,
                preview
                    .prompt
                    .as_deref()
                    .unwrap_or("the most recently abandoned branch"),
                preview.changed_paths.len(),
                preview.changed_paths.join("\n")
            )
        } else if panel == WorkflowPanel::Usage {
            present::usage_detail(&self.presentation_state())
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
        self.workflow.pending = Some(tokio::task::spawn_blocking(move || {
            let preview = if redo {
                manager.prepare_redo(mode)?
            } else if let Some(Action::Checkpoint(id)) = selected {
                manager.prepare_rewind(&id, mode)?
            } else {
                anyhow::bail!("Select a checkpoint first")
            };
            Ok(Loaded::Preview(preview))
        }));
    }

    pub(super) async fn handle_workflow_key(&mut self, key: KeyEvent) {
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
                    let arguments = self.workflow.query.trim().to_string();
                    if arguments.is_empty() {
                        self.workflow.error = Some("An argument is required.".into());
                        return;
                    }
                    self.close_workflow();
                    Box::pin(self.handle_command(&format!("{command} {arguments}"))).await;
                }
                KeyCode::Backspace => {
                    self.workflow.query.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && self.workflow.query.len() < 4096 =>
                {
                    self.workflow.query.push(ch);
                }
                _ => {}
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
            if let Some(preview) = self.workflow.preview.take() {
                self.session
                    .handles()
                    .checkpoints
                    .cancel_preview(&preview.token);
            } else {
                self.close_workflow();
            }
            return;
        }
        if let Some(preview) = self.workflow.preview.as_ref() {
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
            KeyCode::PageUp | KeyCode::PageDown => {
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
                }
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
            KeyCode::Enter => self.activate_workflow_row().await,
            KeyCode::Backspace => {
                self.workflow.query.pop();
                self.workflow.selected = 0;
                self.workflow_query_changed();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && matches!(
                        panel,
                        Some(
                            WorkflowPanel::Actions
                                | WorkflowPanel::Search
                                | WorkflowPanel::Sessions
                        )
                    ) =>
            {
                self.workflow.query.push(ch);
                self.workflow.selected = 0;
                self.workflow_query_changed();
            }
            _ => {}
        }
    }

    fn workflow_query_changed(&mut self) {
        if self.workflow.panel == Some(WorkflowPanel::Sessions) {
            self.workflow.cancel_load();
            self.workflow.search_due = Some(Instant::now() + Duration::from_millis(200));
        }
        self.workflow.scroll.set_offset(0);
    }

    pub(super) fn workflow_paste(&mut self, text: &str) {
        if self.workflow.command_input.is_some()
            || matches!(
                self.workflow.panel,
                Some(WorkflowPanel::Actions | WorkflowPanel::Search | WorkflowPanel::Sessions)
            )
        {
            self.workflow
                .query
                .extend(text.chars().filter(|ch| !ch.is_control()).take(1000));
            self.workflow.selected = 0;
            self.workflow_query_changed();
        }
    }

    async fn activate_workflow_row(&mut self) {
        let Some(action) = self.workflow.selected_row().map(|row| row.action.clone()) else {
            return;
        };
        match action {
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
            | "diff-base"
            | "diff"
            | "timeline"
            | "sessions"
            | "attachments"
            | "context"
            | "actions"
            | "find"
            | "recover"
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

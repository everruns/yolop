//! Durable turn checkpoints and branch-aware session rewind.
//!
//! Conversation events remain append-only in `events.jsonl`. This module keeps
//! a small append-only timeline that selects the active event ranges and stores
//! pre-turn workspace trees in private Git refs. Restores never rewrite the
//! user's branch, HEAD, or index.

use crate::exec::worktree::WorktreeManager;
use crate::runtime::session_log::{
    JsonlEventEmitter, SessionMaterializer, messages_from_events, replay,
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use everruns_core::Event;
use everruns_core::SessionTaskRegistry;
use everruns_provider::typed_id::SessionId;
use everruns_test_support::InMemoryMessageRetriever;
// TM-FS: upstream deprecated `WRITE_BLOCKLIST` in favour of
// `WorkspacePolicy`. Swapping yolop's write-protection over is a
// security-relevant change that deserves its own review rather than riding
// along with the 0.18 migration, so the legacy list stays for now behind a
// single alias instead of an `allow` at every call site.
#[allow(deprecated)]
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

// Yolop's own traversal skip-list: which directories checkpointing and LSP
// indexing walk past. Upstream dropped its exported constant with the
// WorkspacePolicy move; this is deliberately a local list, not the agent's
// write guard — that is still enforced by `WriteBlocklistFileStore` around
// the session file store.
const WRITE_BLOCKLIST: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    ".tox",
    ".gradle",
];

const TIMELINE_FILE: &str = "timeline.jsonl";
const MAX_PROMPT_PREVIEW_CHARS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreMode {
    Conversation,
    Workspace,
    Both,
}

impl RestoreMode {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("both") {
            "conversation" | "chat" => Ok(Self::Conversation),
            "workspace" | "files" | "code" => Ok(Self::Workspace),
            "both" | "all" => Ok(Self::Both),
            other => bail!("unknown restore mode `{other}`; use conversation, workspace, or both"),
        }
    }

    fn includes_conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both)
    }

    fn includes_workspace(self) -> bool {
        matches!(self, Self::Workspace | Self::Both)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkspaceSnapshot {
    tree: String,
    reference: Option<String>,
}

#[derive(Clone, Debug)]
struct CheckpointNode {
    id: String,
    parent: Option<String>,
    prompt: String,
    event_start: i32,
    event_end: Option<i32>,
    snapshot: Option<WorkspaceSnapshot>,
    snapshot_error: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
struct RedoState {
    head: Option<String>,
    snapshot: Option<WorkspaceSnapshot>,
}

#[derive(Clone, Debug, Default)]
struct TimelineState {
    baseline_end: i32,
    nodes: BTreeMap<String, CheckpointNode>,
    head: Option<String>,
    pending: Option<String>,
    redo: Option<RedoState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TimelineRecord {
    Initialized {
        baseline_end: i32,
        created_at: DateTime<Utc>,
    },
    CheckpointCreated {
        id: String,
        parent: Option<String>,
        prompt: String,
        event_start: i32,
        snapshot: Option<WorkspaceSnapshot>,
        snapshot_error: Option<String>,
        created_at: DateTime<Utc>,
    },
    TurnCompleted {
        checkpoint_id: String,
        event_end: i32,
        success: bool,
        completed_at: DateTime<Utc>,
    },
    HeadMoved {
        from: Option<String>,
        to: Option<String>,
        redo_head: Option<String>,
        redo_snapshot: Option<WorkspaceSnapshot>,
        moved_at: DateTime<Utc>,
    },
    RedoCleared {
        cleared_at: DateTime<Utc>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointSummary {
    pub id: String,
    pub prompt: String,
    pub workspace_available: bool,
    pub workspace_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
enum RestoreTarget {
    Rewind { checkpoint_id: String },
    Redo,
}

#[derive(Clone, Debug)]
struct PreparedRestore {
    token: String,
    target: RestoreTarget,
    mode: RestoreMode,
    conversation_head: Option<String>,
    workspace_target: Option<WorkspaceSnapshot>,
    recovery_snapshot: Option<WorkspaceSnapshot>,
    changed_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestorePreview {
    pub token: String,
    pub checkpoint_id: Option<String>,
    pub prompt: Option<String>,
    pub mode: RestoreMode,
    pub changed_paths: Vec<String>,
}

impl RestorePreview {
    pub fn render(&self) -> String {
        let mut lines = vec![match (&self.checkpoint_id, &self.prompt) {
            (Some(id), Some(prompt)) => format!("restore `{id}` before: {prompt}"),
            _ => "restore the most recently abandoned branch".to_string(),
        }];
        if self.mode.includes_workspace() {
            if self.changed_paths.is_empty() {
                lines.push("workspace: no captured file changes".to_string());
            } else {
                lines.push(format!(
                    "workspace: {} path(s) will change",
                    self.changed_paths.len()
                ));
                lines.extend(
                    self.changed_paths
                        .iter()
                        .take(20)
                        .map(|path| format!("  {}", safe_display(path))),
                );
                if self.changed_paths.len() > 20 {
                    lines.push(format!("  … and {} more", self.changed_paths.len() - 20));
                }
            }
        }
        lines.push(format!("confirm with token `{}`", self.token));
        lines.join("\n")
    }
}

pub struct CheckpointManager {
    session_id: SessionId,
    session_dir: PathBuf,
    log_path: PathBuf,
    timeline_path: PathBuf,
    worktree: Arc<WorktreeManager>,
    events: Arc<JsonlEventEmitter>,
    materializer: Arc<SessionMaterializer>,
    messages: Arc<InMemoryMessageRetriever>,
    state: Mutex<TimelineState>,
    prepared: Mutex<Option<PreparedRestore>>,
    queued_confirmation: Mutex<Option<String>>,
    notice: Mutex<Option<String>>,
    restored_prompt: Mutex<Option<String>>,
    task_registry: RwLock<Option<Arc<dyn SessionTaskRegistry>>>,
    snapshot_nonce: AtomicU64,
}

pub struct TurnCheckpoint {
    manager: Arc<CheckpointManager>,
    id: String,
    finished: bool,
}

impl TurnCheckpoint {
    pub fn finish(mut self, success: bool) -> Result<()> {
        self.manager.finish_turn(&self.id, success)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for TurnCheckpoint {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.manager.finish_turn(&self.id, false)
        {
            tracing::warn!(%error, checkpoint = %self.id, "close interrupted checkpoint");
        }
    }
}

impl CheckpointManager {
    pub fn open(
        session_id: SessionId,
        session_dir: PathBuf,
        log_path: PathBuf,
        worktree: Arc<WorktreeManager>,
        events: Arc<JsonlEventEmitter>,
        messages: Arc<InMemoryMessageRetriever>,
        max_sequence: i32,
    ) -> Result<Self> {
        let timeline_path = session_dir.join(TIMELINE_FILE);
        let mut state = load_timeline(&timeline_path)?;
        if !timeline_path.exists() {
            state.baseline_end = max_sequence;
        } else if state.nodes.is_empty() && max_sequence > state.baseline_end {
            // Older/internal callers can write a turn through the raw runtime
            // before checkpointing owns the first boundary. Preserve that
            // legacy prefix instead of hiding it on the next resume.
            let record = TimelineRecord::Initialized {
                baseline_end: max_sequence,
                created_at: Utc::now(),
            };
            append_record(&timeline_path, &record)?;
            apply_record(&mut state, record);
        }
        let manager = Self {
            session_id,
            session_dir,
            log_path,
            timeline_path,
            worktree,
            materializer: events.materializer(),
            events,
            messages,
            state: Mutex::new(state),
            prepared: Mutex::new(None),
            queued_confirmation: Mutex::new(None),
            notice: Mutex::new(None),
            restored_prompt: Mutex::new(None),
            task_registry: RwLock::new(None),
            snapshot_nonce: AtomicU64::new(0),
        };
        manager.close_interrupted_turn()?;
        Ok(manager)
    }

    pub fn filter_active_events(&self, events: Vec<Event>) -> Vec<Event> {
        let state = self.state.lock().expect("checkpoint state lock");
        filter_active_events(&state, events)
    }

    /// Whether an event boundary belongs to the currently selected timeline.
    ///
    /// Durable model-context checkpoints use this to ignore replacements made
    /// on an abandoned conversation branch without deleting append-only data.
    pub fn is_event_sequence_active(&self, sequence: i64) -> bool {
        let Ok(sequence) = i32::try_from(sequence) else {
            return false;
        };
        if sequence > self.events.last_sequence() {
            return false;
        }
        let state = self.state.lock().expect("checkpoint state lock");
        event_sequence_is_active(&state, sequence)
    }

    pub fn begin_turn(&self, prompt: &str) -> Result<String> {
        self.materializer.ensure()?;
        self.close_interrupted_turn()?;
        if let Some(prepared) = self.prepared.lock().expect("prepared restore lock").take() {
            self.delete_snapshot_ref(prepared.recovery_snapshot.as_ref());
        }
        let mut state = self.state.lock().expect("checkpoint state lock");
        let persisted_sequence = self.events.last_sequence();
        if !self.timeline_path.exists() {
            let record = TimelineRecord::Initialized {
                baseline_end: state.baseline_end.max(persisted_sequence),
                created_at: Utc::now(),
            };
            append_record(&self.timeline_path, &record)?;
            apply_record(&mut state, record);
        } else if state.nodes.is_empty() && persisted_sequence > state.baseline_end {
            let record = TimelineRecord::Initialized {
                baseline_end: persisted_sequence,
                created_at: Utc::now(),
            };
            append_record(&self.timeline_path, &record)?;
            apply_record(&mut state, record);
        }
        if let Some(redo) = state.redo.take() {
            append_record(
                &self.timeline_path,
                &TimelineRecord::RedoCleared {
                    cleared_at: Utc::now(),
                },
            )?;
            self.delete_snapshot_ref(redo.snapshot.as_ref());
        }
        let event_start = persisted_sequence.saturating_add(1);
        let id = format!("cp_{event_start}_{}", state.nodes.len() + 1);
        let (snapshot, snapshot_error) = match self.capture_workspace(&id, true) {
            Ok(snapshot) => (snapshot, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let record = TimelineRecord::CheckpointCreated {
            id: id.clone(),
            parent: state.head.clone(),
            prompt: prompt.to_string(),
            event_start,
            snapshot: snapshot.clone(),
            snapshot_error: snapshot_error.clone(),
            created_at: Utc::now(),
        };
        append_record(&self.timeline_path, &record)?;
        apply_record(&mut state, record);
        Ok(id)
    }

    pub fn start_turn(self: &Arc<Self>, prompt: &str) -> Result<TurnCheckpoint> {
        Ok(TurnCheckpoint {
            manager: self.clone(),
            id: self.begin_turn(prompt)?,
            finished: false,
        })
    }

    pub fn finish_turn(&self, checkpoint_id: &str, success: bool) -> Result<()> {
        let record = TimelineRecord::TurnCompleted {
            checkpoint_id: checkpoint_id.to_string(),
            event_end: self.events.last_sequence(),
            success,
            completed_at: Utc::now(),
        };
        append_record(&self.timeline_path, &record)?;
        apply_record(
            &mut self.state.lock().expect("checkpoint state lock"),
            record,
        );
        Ok(())
    }

    pub fn list(&self) -> Vec<CheckpointSummary> {
        let state = self.state.lock().expect("checkpoint state lock");
        active_path(&state)
            .into_iter()
            .rev()
            .filter_map(|id| state.nodes.get(&id))
            .map(|node| CheckpointSummary {
                id: node.id.clone(),
                prompt: prompt_preview(&node.prompt),
                workspace_available: node.snapshot.is_some(),
                workspace_error: node.snapshot_error.as_deref().map(safe_display),
                created_at: node.created_at,
            })
            .collect()
    }

    pub fn prepare_undo(&self, mode: RestoreMode) -> Result<RestorePreview> {
        let checkpoint_id = self
            .state
            .lock()
            .expect("checkpoint state lock")
            .head
            .clone()
            .ok_or_else(|| anyhow!("nothing to undo"))?;
        self.prepare_rewind(&checkpoint_id, mode)
    }

    pub fn default_undo_mode(&self) -> RestoreMode {
        let state = self.state.lock().expect("checkpoint state lock");
        state
            .head
            .as_ref()
            .and_then(|head| state.nodes.get(head))
            .map_or(RestoreMode::Conversation, |node| {
                if node.snapshot.is_some() {
                    RestoreMode::Both
                } else {
                    RestoreMode::Conversation
                }
            })
    }

    pub fn default_rewind_mode(&self, checkpoint_id: &str) -> RestoreMode {
        self.state
            .lock()
            .expect("checkpoint state lock")
            .nodes
            .get(checkpoint_id)
            .map_or(RestoreMode::Conversation, |node| {
                if node.snapshot.is_some() {
                    RestoreMode::Both
                } else {
                    RestoreMode::Conversation
                }
            })
    }

    pub fn default_redo_mode(&self) -> RestoreMode {
        if self
            .state
            .lock()
            .expect("checkpoint state lock")
            .redo
            .as_ref()
            .and_then(|redo| redo.snapshot.as_ref())
            .is_some()
        {
            RestoreMode::Both
        } else {
            RestoreMode::Conversation
        }
    }

    pub fn prepare_rewind(&self, checkpoint_id: &str, mode: RestoreMode) -> Result<RestorePreview> {
        let (node, conversation_head) = {
            let state = self.state.lock().expect("checkpoint state lock");
            let active = active_path(&state);
            if !active.iter().any(|id| id == checkpoint_id) {
                bail!("checkpoint `{checkpoint_id}` is not on the active branch");
            }
            let node = state
                .nodes
                .get(checkpoint_id)
                .cloned()
                .ok_or_else(|| anyhow!("checkpoint `{checkpoint_id}` not found"))?;
            (node.clone(), node.parent.clone())
        };
        let (recovery_snapshot, changed_paths) = self.prepare_workspace(
            mode,
            node.snapshot.as_ref(),
            &format!("recovery_{}", self.events.last_sequence()),
        )?;
        let token = confirmation_token(self.events.last_sequence());
        self.set_prepared(PreparedRestore {
            token: token.clone(),
            target: RestoreTarget::Rewind {
                checkpoint_id: node.id.clone(),
            },
            mode,
            conversation_head,
            workspace_target: node.snapshot.clone(),
            recovery_snapshot,
            changed_paths: changed_paths.clone(),
        });
        Ok(RestorePreview {
            token,
            checkpoint_id: Some(node.id),
            prompt: Some(prompt_preview(&node.prompt)),
            mode,
            changed_paths,
        })
    }

    pub fn prepare_redo(&self, mode: RestoreMode) -> Result<RestorePreview> {
        let redo = self
            .state
            .lock()
            .expect("checkpoint state lock")
            .redo
            .clone()
            .ok_or_else(|| anyhow!("nothing to redo"))?;
        let (recovery_snapshot, changed_paths) = self.prepare_workspace(
            mode,
            redo.snapshot.as_ref(),
            &format!("recovery_{}", self.events.last_sequence()),
        )?;
        let token = confirmation_token(self.events.last_sequence());
        self.set_prepared(PreparedRestore {
            token: token.clone(),
            target: RestoreTarget::Redo,
            mode,
            conversation_head: redo.head,
            workspace_target: redo.snapshot,
            recovery_snapshot,
            changed_paths: changed_paths.clone(),
        });
        Ok(RestorePreview {
            token,
            checkpoint_id: None,
            prompt: None,
            mode,
            changed_paths,
        })
    }

    pub async fn confirm(&self, token: &str) -> Result<String> {
        self.ensure_no_active_tasks().await?;
        let prepared = self
            .prepared
            .lock()
            .expect("prepared restore lock")
            .take()
            .ok_or_else(|| anyhow!("no restore is awaiting confirmation"))?;
        if prepared.token != token {
            *self.prepared.lock().expect("prepared restore lock") = Some(prepared);
            bail!("confirmation token does not match the pending restore");
        }
        let is_redo = matches!(&prepared.target, RestoreTarget::Redo);

        let restored_prompt = match &prepared.target {
            RestoreTarget::Rewind { checkpoint_id } if prepared.mode.includes_conversation() => {
                self.active_prompt_for(checkpoint_id)
            }
            _ => None,
        };

        if prepared.mode.includes_workspace() {
            let current = self
                .capture_workspace(&format!("verify_{}", self.events.last_sequence()), false)?
                .ok_or_else(|| anyhow!("workspace restore is unavailable"))?;
            let recovery = prepared
                .recovery_snapshot
                .as_ref()
                .ok_or_else(|| anyhow!("recovery snapshot is missing"))?;
            if current.tree != recovery.tree {
                self.delete_snapshot_ref(prepared.recovery_snapshot.as_ref());
                bail!("workspace changed after preview; request a fresh restore preview");
            }
            let target = prepared
                .workspace_target
                .as_ref()
                .ok_or_else(|| anyhow!("selected checkpoint has no workspace snapshot"))?;
            if let Err(error) = self.restore_workspace(target) {
                let _ = self.restore_workspace(recovery);
                return Err(error.context("restore workspace"));
            }
        }

        {
            let mut state = self.state.lock().expect("checkpoint state lock");
            let from = state.head.clone();
            let to = if prepared.mode.includes_conversation() {
                prepared.conversation_head.clone()
            } else {
                from.clone()
            };
            let record = TimelineRecord::HeadMoved {
                from: from.clone(),
                to,
                redo_head: from,
                redo_snapshot: prepared.recovery_snapshot.clone(),
                moved_at: Utc::now(),
            };
            append_record(&self.timeline_path, &record)?;
            apply_record(&mut state, record);
            if is_redo {
                let cleared = TimelineRecord::RedoCleared {
                    cleared_at: Utc::now(),
                };
                append_record(&self.timeline_path, &cleared)?;
                apply_record(&mut state, cleared);
            }
        }

        if prepared.mode.includes_conversation() {
            self.reseed_active_conversation().await?;
            *self.restored_prompt.lock().expect("restored prompt lock") = restored_prompt;
        }
        let action = match prepared.target {
            RestoreTarget::Rewind { checkpoint_id } => format!("restored `{checkpoint_id}`"),
            RestoreTarget::Redo => "restored the abandoned branch".to_string(),
        };
        if is_redo {
            self.delete_snapshot_ref(prepared.workspace_target.as_ref());
            self.delete_snapshot_ref(prepared.recovery_snapshot.as_ref());
        }
        Ok(format!(
            "{action} ({:?}; {} workspace path(s))",
            prepared.mode,
            prepared.changed_paths.len()
        ))
    }

    pub fn queue_confirmation(&self, token: &str) -> Result<()> {
        let prepared = self.prepared.lock().expect("prepared restore lock");
        let Some(prepared) = prepared.as_ref() else {
            bail!("no restore is awaiting confirmation");
        };
        if prepared.token != token {
            bail!("confirmation token does not match the pending restore");
        }
        *self
            .queued_confirmation
            .lock()
            .expect("queued confirmation lock") = Some(token.to_string());
        Ok(())
    }

    pub async fn apply_queued_confirmation(&self) {
        let token = self
            .queued_confirmation
            .lock()
            .expect("queued confirmation lock")
            .take();
        let Some(token) = token else {
            return;
        };
        let notice = match self.confirm(&token).await {
            Ok(message) => message,
            Err(error) => format!(
                "checkpoint restore failed: {}",
                safe_display(&format!("{error:#}"))
            ),
        };
        *self.notice.lock().expect("checkpoint notice lock") = Some(notice);
    }

    pub fn take_notice(&self) -> Option<String> {
        self.notice.lock().ok()?.take()
    }

    pub fn take_restored_prompt(&self) -> Option<String> {
        self.restored_prompt.lock().ok()?.take()
    }

    pub fn attach_task_registry(&self, registry: Arc<dyn SessionTaskRegistry>) {
        if let Ok(mut slot) = self.task_registry.write() {
            *slot = Some(registry);
        }
    }

    pub fn active_prompt_for(&self, checkpoint_id: &str) -> Option<String> {
        self.state
            .lock()
            .ok()?
            .nodes
            .get(checkpoint_id)
            .map(|node| node.prompt.clone())
    }

    async fn reseed_active_conversation(&self) -> Result<()> {
        let replayed = replay(&self.log_path, self.session_id)?;
        let active = self.filter_active_events(replayed.events);
        let messages = messages_from_events(&active);
        self.events.replace_collected_events(active).await;
        self.messages.seed(self.session_id, messages).await;
        Ok(())
    }

    async fn ensure_no_active_tasks(&self) -> Result<()> {
        let registry = self
            .task_registry
            .read()
            .ok()
            .and_then(|registry| registry.clone());
        let Some(registry) = registry else {
            return Ok(());
        };
        let active = registry
            .list(self.session_id, None)
            .await?
            .into_iter()
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.id)
            .collect::<Vec<_>>();
        if !active.is_empty() {
            bail!(
                "cannot restore while session tasks are active: {}",
                active.join(", ")
            );
        }
        Ok(())
    }

    fn close_interrupted_turn(&self) -> Result<()> {
        let pending = self
            .state
            .lock()
            .expect("checkpoint state lock")
            .pending
            .clone();
        if let Some(id) = pending {
            self.finish_turn(&id, false)?;
        }
        Ok(())
    }

    fn prepare_workspace(
        &self,
        mode: RestoreMode,
        target: Option<&WorkspaceSnapshot>,
        recovery_name: &str,
    ) -> Result<(Option<WorkspaceSnapshot>, Vec<String>)> {
        if !mode.includes_workspace() {
            return Ok((None, Vec::new()));
        }
        let target =
            target.ok_or_else(|| anyhow!("selected checkpoint has no workspace snapshot"))?;
        let nonce = self.snapshot_nonce.fetch_add(1, Ordering::Relaxed);
        let recovery_name = format!("{recovery_name}_{nonce}");
        let recovery = self
            .capture_workspace(&recovery_name, true)?
            .ok_or_else(|| anyhow!("workspace restore is available only in a Yolop worktree"))?;
        let changed = self.diff_trees(&recovery.tree, &target.tree)?;
        Ok((Some(recovery), changed))
    }

    fn capture_workspace(&self, name: &str, anchor: bool) -> Result<Option<WorkspaceSnapshot>> {
        let Some(info) = self.worktree.owned_worktree_info() else {
            return Ok(None);
        };
        let index = self.session_dir.join(format!("checkpoint-{name}.index"));
        let _ = std::fs::remove_file(&index);
        let result = (|| {
            run_git(
                git_command(&info.path, Some(&index)).args(["read-tree", "HEAD"]),
                "initialize checkpoint index",
            )?;
            let mut add = git_command(&info.path, Some(&index));
            add.args(["add", "-A", "--", "."]);
            for blocked in WRITE_BLOCKLIST {
                add.arg(format!(":(exclude,glob)**/{blocked}/**"));
                add.arg(format!(":(exclude,glob){blocked}/**"));
            }
            run_git(&mut add, "snapshot workspace")?;
            let mut remove_blocked = git_command(&info.path, Some(&index));
            remove_blocked.args(["rm", "-r", "-f", "--cached", "--ignore-unmatch", "--"]);
            for blocked in WRITE_BLOCKLIST {
                remove_blocked.arg(format!(":(glob)**/{blocked}/**"));
                remove_blocked.arg(format!(":(glob){blocked}/**"));
            }
            run_git(&mut remove_blocked, "exclude protected checkpoint paths")?;
            let entries = git_output_bytes(
                git_command(&info.path, Some(&index)).args(["ls-files", "--stage", "-z"]),
                "validate checkpoint paths",
            )?;
            for entry in entries
                .split(|byte| *byte == 0)
                .filter(|entry| !entry.is_empty())
            {
                let separator = entry
                    .iter()
                    .position(|byte| *byte == b'\t')
                    .ok_or_else(|| anyhow!("invalid Git index entry"))?;
                let (metadata, path_with_separator) = entry.split_at(separator);
                let path = &path_with_separator[1..];
                std::str::from_utf8(path)
                    .context("checkpoint restore does not support non-UTF-8 paths")?;
                if metadata.starts_with(b"160000 ") {
                    bail!("checkpoint restore does not support submodules");
                }
            }
            let tree = git_output(
                git_command(&info.path, Some(&index)).args(["write-tree"]),
                "write checkpoint tree",
            )?;
            let reference =
                anchor.then(|| format!("refs/yolop/checkpoints/{}/{name}", self.session_id));
            if let Some(reference) = &reference {
                run_git(
                    git_command(&info.path, None).args(["update-ref", reference, &tree]),
                    "anchor checkpoint tree",
                )?;
            }
            Ok(WorkspaceSnapshot { tree, reference })
        })();
        let _ = std::fs::remove_file(index);
        result.map(Some)
    }

    fn delete_snapshot_ref(&self, snapshot: Option<&WorkspaceSnapshot>) {
        let Some(reference) = snapshot.and_then(|snapshot| snapshot.reference.as_deref()) else {
            return;
        };
        let Some(info) = self.worktree.owned_worktree_info() else {
            return;
        };
        let _ = run_git(
            git_command(&info.path, None).args(["update-ref", "-d", reference]),
            "delete checkpoint ref",
        );
    }

    fn set_prepared(&self, prepared: PreparedRestore) {
        let replaced = self
            .prepared
            .lock()
            .expect("prepared restore lock")
            .replace(prepared);
        if let Some(replaced) = replaced {
            self.delete_snapshot_ref(replaced.recovery_snapshot.as_ref());
        }
    }

    fn diff_trees(&self, current: &str, target: &str) -> Result<Vec<String>> {
        let info = self
            .worktree
            .owned_worktree_info()
            .ok_or_else(|| anyhow!("workspace restore is unavailable"))?;
        let output = git_output_bytes(
            git_command(&info.path, None).args([
                "diff",
                "--name-status",
                "-z",
                "--no-renames",
                current,
                target,
            ]),
            "preview checkpoint restore",
        )?;
        let fields: Vec<&[u8]> = output
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect();
        if !fields.len().is_multiple_of(2) {
            bail!("invalid Git name-status output");
        }
        fields
            .chunks_exact(2)
            .map(|pair| {
                String::from_utf8(pair[1].to_vec())
                    .context("checkpoint restore does not support non-UTF-8 paths")
            })
            .collect()
    }

    fn restore_workspace(&self, target: &WorkspaceSnapshot) -> Result<()> {
        let info = self
            .worktree
            .owned_worktree_info()
            .ok_or_else(|| anyhow!("workspace restore is unavailable"))?;
        let current = self
            .capture_workspace(
                &format!("restore_current_{}", self.events.last_sequence()),
                false,
            )?
            .ok_or_else(|| anyhow!("workspace restore is unavailable"))?;
        let current_paths = tree_paths(&info.path, &current.tree)?;
        let target_paths = tree_paths(&info.path, &target.tree)?;
        let removable = current_paths
            .difference(&target_paths)
            .cloned()
            .collect::<BTreeSet<_>>();
        for path in &target_paths {
            validate_restore_path(&info.path, path, &removable)?;
        }
        for path in &removable {
            remove_captured_path(&info.path, path)?;
        }

        let index = self.session_dir.join("checkpoint-restore.index");
        let _ = std::fs::remove_file(&index);
        run_git(
            git_command(&info.path, Some(&index)).args(["read-tree", &target.tree]),
            "load checkpoint tree",
        )?;
        run_git(
            git_command(&info.path, Some(&index)).args(["checkout-index", "-a", "-f"]),
            "materialize checkpoint tree",
        )?;
        let _ = std::fs::remove_file(index);

        let verified = self
            .capture_workspace(&format!("restored_{}", self.events.last_sequence()), false)?
            .ok_or_else(|| anyhow!("workspace restore verification unavailable"))?;
        if verified.tree != target.tree {
            bail!("workspace restore verification failed");
        }
        Ok(())
    }
}

fn filter_active_events(state: &TimelineState, events: Vec<Event>) -> Vec<Event> {
    events
        .into_iter()
        .filter(|event| {
            event
                .sequence
                .is_none_or(|sequence| event_sequence_is_active(state, sequence))
        })
        .collect()
}

fn event_sequence_is_active(state: &TimelineState, sequence: i32) -> bool {
    if sequence <= state.baseline_end {
        return true;
    }
    if state.pending.as_ref().is_some_and(|id| {
        state.nodes.get(id).is_some_and(|node| {
            sequence >= node.event_start && node.event_end.is_none_or(|end| sequence <= end)
        })
    }) {
        return true;
    }
    active_path(state).into_iter().any(|id| {
        state.nodes.get(&id).is_some_and(|node| {
            sequence >= node.event_start && node.event_end.is_none_or(|end| sequence <= end)
        })
    })
}

fn active_path(state: &TimelineState) -> Vec<String> {
    let mut reversed = Vec::new();
    let mut cursor = state.head.clone();
    let mut seen = BTreeSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(node) = state.nodes.get(&id) else {
            break;
        };
        reversed.push(id);
        cursor = node.parent.clone();
    }
    reversed.reverse();
    reversed
}

fn load_timeline(path: &Path) -> Result<TimelineState> {
    if !path.exists() {
        return Ok(TimelineState::default());
    }
    let mut file = File::open(path).with_context(|| format!("open timeline {}", path.display()))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .with_context(|| format!("read timeline {}", path.display()))?;
    let lines: Vec<&[u8]> = contents.split(|byte| *byte == b'\n').collect();
    let last_nonempty = lines
        .iter()
        .rposition(|line| !line.iter().all(u8::is_ascii_whitespace));
    let mut state = TimelineState::default();
    for (index, line) in lines.into_iter().enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: TimelineRecord = match serde_json::from_slice(line) {
            Ok(record) => record,
            Err(_) if Some(index) == last_nonempty => break,
            Err(error) => {
                return Err(error).with_context(|| format!("parse timeline line {}", index + 1));
            }
        };
        apply_record(&mut state, record);
    }
    Ok(state)
}

fn apply_record(state: &mut TimelineState, record: TimelineRecord) {
    match record {
        TimelineRecord::Initialized { baseline_end, .. } => state.baseline_end = baseline_end,
        TimelineRecord::CheckpointCreated {
            id,
            parent,
            prompt,
            event_start,
            snapshot,
            snapshot_error,
            created_at,
        } => {
            state.pending = Some(id.clone());
            state.nodes.insert(
                id.clone(),
                CheckpointNode {
                    id,
                    parent,
                    prompt,
                    event_start,
                    event_end: None,
                    snapshot,
                    snapshot_error,
                    created_at,
                },
            );
        }
        TimelineRecord::TurnCompleted {
            checkpoint_id,
            event_end,
            ..
        } => {
            if let Some(node) = state.nodes.get_mut(&checkpoint_id) {
                node.event_end = Some(event_end);
                state.head = Some(checkpoint_id.clone());
            }
            if state.pending.as_deref() == Some(&checkpoint_id) {
                state.pending = None;
            }
        }
        TimelineRecord::HeadMoved {
            to,
            redo_head,
            redo_snapshot,
            ..
        } => {
            state.head = to;
            state.redo = Some(RedoState {
                head: redo_head,
                snapshot: redo_snapshot,
            });
        }
        TimelineRecord::RedoCleared { .. } => state.redo = None,
    }
}

fn append_record(path: &Path, record: &TimelineRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut file, record)?;
    file.write_all(b"\n")?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

fn git_command(worktree: &Path, index: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(worktree);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    command
}

fn run_git(command: &mut Command, label: &str) -> Result<()> {
    let output = command.output().with_context(|| label.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{label}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn git_output(command: &mut Command, label: &str) -> Result<String> {
    Ok(String::from_utf8(git_output_bytes(command, label)?)?
        .trim()
        .to_string())
}

fn git_output_bytes(command: &mut Command, label: &str) -> Result<Vec<u8>> {
    let output = command.output().with_context(|| label.to_string())?;
    if !output.status.success() {
        bail!(
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn tree_paths(worktree: &Path, tree: &str) -> Result<BTreeSet<String>> {
    let output = git_output_bytes(
        git_command(worktree, None).args(["ls-tree", "-rz", "--name-only", tree]),
        "list checkpoint tree",
    )?;
    output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| {
            String::from_utf8(field.to_vec())
                .context("checkpoint restore does not support non-UTF-8 paths")
        })
        .collect()
}

fn remove_captured_path(root: &Path, relative: &str) -> Result<()> {
    validate_relative_path(relative)?;
    let path = Path::new(relative);
    let absolute = root.join(path);
    match std::fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir(&absolute)?,
        Ok(_) => std::fs::remove_file(&absolute)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut parent = absolute.parent();
    while let Some(directory) = parent {
        if directory == root {
            break;
        }
        match std::fs::remove_dir(directory) {
            Ok(()) => parent = directory.parent(),
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                parent = directory.parent()
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_relative_path(relative: &str) -> Result<()> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || path.components().any(|component| {
            matches!(component, Component::Normal(name) if WRITE_BLOCKLIST.iter().any(|blocked| name == std::ffi::OsStr::new(blocked)))
        })
    {
        bail!("unsafe checkpoint path `{relative}`");
    }
    Ok(())
}

fn validate_restore_path(root: &Path, relative: &str, removable: &BTreeSet<String>) -> Result<()> {
    validate_relative_path(relative)?;
    let path = Path::new(relative);
    let mut current = root.to_path_buf();
    let mut current_relative = PathBuf::new();
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    for component in parent.components() {
        if let Component::Normal(name) = component {
            current.push(name);
            current_relative.push(name);
            let is_removable = current_relative
                .to_str()
                .is_some_and(|path| removable.contains(path));
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() && !is_removable => {
                    bail!("checkpoint path crosses symlink `{}`", current.display());
                }
                Ok(metadata) if !metadata.is_dir() && !is_removable => {
                    bail!(
                        "checkpoint path parent is not a directory `{}`",
                        current.display()
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn confirmation_token(sequence: i32) -> String {
    format!("restore-{sequence}-{}", Utc::now().timestamp_millis())
}

fn prompt_preview(prompt: &str) -> String {
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let preview: String = chars.by_ref().take(MAX_PROMPT_PREVIEW_CHARS).collect();
    let preview = safe_display(&preview);
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

pub(crate) fn safe_display(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorktreesMode;
    use crate::runtime::session_log::{JsonlEventEmitter, session_log_path};
    use everruns_core::EventEmitter;
    use everruns_core::Message;
    use everruns_core::{EventContext, EventRequest, InputMessageData};
    use tempfile::TempDir;

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn manager() -> (TempDir, TempDir, Arc<CheckpointManager>) {
        let root = tempfile::tempdir().expect("root");
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.name", "Test User"]);
        git(root.path(), &["config", "user.email", "test@example.com"]);
        std::fs::write(root.path().join("tracked.txt"), "base\n").expect("base file");
        git(root.path(), &["add", "tracked.txt"]);
        git(root.path(), &["commit", "-qm", "base"]);

        let session_id = SessionId::new();
        let session = tempfile::tempdir().expect("session");
        let session_dir = session.path().to_path_buf();
        let log_path = session_log_path(&session_dir);
        let events = Arc::new(JsonlEventEmitter::open(&log_path, 1).expect("event emitter"));
        let messages = Arc::new(InMemoryMessageRetriever::new());
        let info = crate::exec::worktree::WorktreeInfo {
            path: root.path().to_path_buf(),
            branch: "master".to_string(),
            base_ref: "HEAD".to_string(),
            slug: "test".to_string(),
        };
        let worktree = Arc::new(WorktreeManager::new(
            WorktreesMode::Always,
            Some(root.path().join("main-checkout")),
            root.path().to_path_buf(),
            session_id,
            session_dir.clone(),
            Some(info),
        ));
        let manager = Arc::new(
            CheckpointManager::open(
                session_id,
                session_dir,
                log_path,
                worktree,
                events,
                messages,
                0,
            )
            .expect("checkpoint manager"),
        );
        (root, session, manager)
    }

    async fn emit_user(manager: &CheckpointManager, text: &str) {
        manager
            .events
            .emit(EventRequest::new(
                manager.session_id,
                EventContext::empty(),
                InputMessageData::new(Message::user(text)),
            ))
            .await
            .expect("emit user");
    }

    #[tokio::test]
    async fn current_turn_event_boundary_is_active_before_turn_finishes() {
        let (_root, _session, manager) = manager().await;
        let current = manager.begin_turn("current prompt").expect("checkpoint");
        emit_user(&manager, "current prompt").await;
        let current_sequence = manager
            .events
            .collected_events()
            .await
            .into_iter()
            .filter_map(|event| event.sequence)
            .max()
            .expect("current turn event sequence");

        assert!(
            manager.is_event_sequence_active(i64::from(current_sequence)),
            "durable context checkpoints must be installable during the active turn"
        );
        assert!(
            !manager.is_event_sequence_active(i64::from(current_sequence) + 1),
            "an open turn must not make a future event boundary active"
        );
        manager
            .finish_turn(&current, true)
            .expect("finish current turn");
    }

    #[tokio::test]
    async fn rewind_filters_abandoned_events_and_restores_files() {
        let (root, _session, manager) = manager().await;
        let first = manager
            .begin_turn("first prompt")
            .expect("first checkpoint");
        std::fs::write(root.path().join("tracked.txt"), "first\n").expect("first edit");
        std::fs::write(root.path().join("new.txt"), "new\n").expect("new file");
        emit_user(&manager, "first prompt").await;
        manager.finish_turn(&first, true).expect("finish first");

        let second = manager
            .begin_turn("second prompt")
            .expect("second checkpoint");
        std::fs::write(root.path().join("tracked.txt"), "second\n").expect("second edit");
        emit_user(&manager, "second prompt").await;
        manager.finish_turn(&second, true).expect("finish second");
        let second_sequence = manager
            .events
            .collected_events()
            .await
            .into_iter()
            .filter_map(|event| event.sequence)
            .max()
            .expect("second turn event sequence");
        assert!(manager.is_event_sequence_active(i64::from(second_sequence)));

        let preview = manager
            .prepare_rewind(&second, RestoreMode::Both)
            .expect("preview rewind");
        assert!(preview.changed_paths.contains(&"tracked.txt".to_string()));
        manager
            .confirm(&preview.token)
            .await
            .expect("confirm rewind");

        assert_eq!(
            std::fs::read_to_string(root.path().join("tracked.txt")).expect("tracked"),
            "first\n"
        );
        assert!(root.path().join("new.txt").exists());
        let active = manager.events.collected_events().await;
        let texts = messages_from_events(&active)
            .into_iter()
            .filter_map(|message| message.text().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["first prompt"]);
        assert!(!manager.is_event_sequence_active(i64::from(second_sequence)));

        let redo = manager
            .prepare_redo(RestoreMode::Both)
            .expect("preview redo");
        manager.confirm(&redo.token).await.expect("confirm redo");
        assert_eq!(
            std::fs::read_to_string(root.path().join("tracked.txt")).expect("tracked"),
            "second\n"
        );
        let active = manager.events.collected_events().await;
        let texts = messages_from_events(&active)
            .into_iter()
            .filter_map(|message| message.text().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["first prompt", "second prompt"]);
        assert!(manager.is_event_sequence_active(i64::from(second_sequence)));
        assert!(manager.prepare_redo(RestoreMode::Both).is_err());
    }

    #[tokio::test]
    async fn first_checkpoint_persists_a_preexisting_event_baseline() {
        let (_root, session, manager) = manager().await;
        emit_user(&manager, "event before checkpointing").await;

        manager.begin_turn("first checkpoint").expect("begin turn");

        let state = load_timeline(&session.path().join(TIMELINE_FILE)).expect("load timeline");
        assert_eq!(state.baseline_end, 1);
        let active = manager.filter_active_events(manager.events.collected_events().await);
        assert_eq!(active.len(), 1, "pre-checkpoint history must remain active");
    }

    #[tokio::test]
    async fn stale_preview_refuses_to_overwrite_new_changes() {
        let (root, _session, manager) = manager().await;
        let first = manager.begin_turn("first").expect("checkpoint");
        std::fs::write(root.path().join("tracked.txt"), "changed\n").expect("edit");
        emit_user(&manager, "first").await;
        manager.finish_turn(&first, true).expect("finish");

        let preview = manager
            .prepare_undo(RestoreMode::Both)
            .expect("prepare undo");
        std::fs::write(root.path().join("tracked.txt"), "manual\n").expect("manual edit");
        let error = manager
            .confirm(&preview.token)
            .await
            .expect_err("stale preview must fail");
        assert!(
            error
                .to_string()
                .contains("workspace changed after preview")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("tracked.txt")).expect("tracked"),
            "manual\n"
        );
    }

    #[tokio::test]
    async fn checkpoint_ids_stay_unique_when_turn_emits_no_events() {
        let (_root, _session, manager) = manager().await;
        let first = manager.begin_turn("first").expect("first checkpoint");
        manager.finish_turn(&first, false).expect("finish first");
        let second = manager.begin_turn("second").expect("second checkpoint");
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn restore_handles_paths_containing_newlines() {
        let (root, _session, manager) = manager().await;
        let checkpoint = manager.begin_turn("create file").expect("checkpoint");
        let unusual = "line\nbreak.txt";
        std::fs::write(root.path().join(unusual), "content").expect("unusual file");
        emit_user(&manager, "create file").await;
        manager.finish_turn(&checkpoint, true).expect("finish");

        let preview = manager
            .prepare_undo(RestoreMode::Both)
            .expect("preview undo");
        assert!(preview.changed_paths.contains(&unusual.to_string()));
        manager.confirm(&preview.token).await.expect("confirm undo");
        assert!(!root.path().join(unusual).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_refuses_to_cross_an_ignored_symlink() {
        let (root, _session, manager) = manager().await;
        std::fs::create_dir(root.path().join("dir")).expect("tracked dir");
        std::fs::write(root.path().join("dir/file.txt"), "inside").expect("tracked child");
        std::fs::write(root.path().join(".gitignore"), "dir\n").expect("gitignore");
        git(root.path(), &["add", ".gitignore"]);
        git(root.path(), &["add", "-f", "dir/file.txt"]);
        git(root.path(), &["commit", "-qm", "tracked child"]);

        let checkpoint = manager.begin_turn("replace dir").expect("checkpoint");
        std::fs::remove_dir_all(root.path().join("dir")).expect("remove tracked dir");
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), root.path().join("dir"))
            .expect("ignored symlink");
        emit_user(&manager, "replace dir").await;
        manager.finish_turn(&checkpoint, true).expect("finish");

        let preview = manager
            .prepare_undo(RestoreMode::Both)
            .expect("preview undo");
        let error = manager
            .confirm(&preview.token)
            .await
            .expect_err("symlink escape must fail");
        let error_chain = format!("{error:#}");
        assert!(
            error_chain.contains("crosses symlink"),
            "unexpected restore error: {error_chain}"
        );
        assert!(root.path().join("dir").is_symlink());
        assert!(
            std::fs::read_dir(outside.path())
                .expect("outside contents")
                .next()
                .is_none()
        );
    }

    #[test]
    fn timeline_ignores_only_a_corrupt_tail() {
        let dir = tempfile::tempdir().expect("timeline dir");
        let path = dir.path().join(TIMELINE_FILE);
        let initialized = TimelineRecord::Initialized {
            baseline_end: 7,
            created_at: Utc::now(),
        };
        std::fs::write(
            &path,
            format!(
                "{}\n{{partial",
                serde_json::to_string(&initialized).unwrap()
            ),
        )
        .expect("write timeline");
        assert_eq!(load_timeline(&path).expect("load tail").baseline_end, 7);

        std::fs::write(
            &path,
            format!(
                "{{broken}}\n{}\n",
                serde_json::to_string(&initialized).unwrap()
            ),
        )
        .expect("write corrupt prefix");
        assert!(load_timeline(&path).is_err());
    }
}

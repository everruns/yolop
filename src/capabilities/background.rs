// The `background` capability — generic background execution for yolop.
//
// A *background task* is a unit of work that runs detached from the foreground
// turn: it has an id, a kind, a lifecycle status, captured output, and is
// cancellable and observable. Two kinds share this one registry, record, status
// model, persistence, and surfaces: `script` (a shell command that outlives the
// turn, e.g. `gh pr checks --watch` waiting on CI) and `agent` (a child session
// that runs one focused turn and returns its final message). See
// specs/background.md.
//
// Durability reuses the per-session folder that `session_log.rs` already owns:
// the registry persists an index to `<session_dir>/background/index.json` and
// each task streams its output to `<session_dir>/background/<id>.log`. On a
// restart the index is restored verbatim, except tasks still marked `running`
// (whose OS process died with the previous yolop) are re-labelled `interrupted`.
// Results survive a restart; in-flight processes do not. See specs/background.md.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::command::{
    CommandDescriptor, CommandExecutionContext, CommandResult, CommandSource, ExecuteCommandRequest,
};
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

pub(crate) const BACKGROUND_CAPABILITY_ID: &str = "background";

const BACKGROUND_SUBDIR: &str = "background";
const INDEX_FILE: &str = "index.json";
/// Per-task output log cap. Generous for a CI watch; protects the session
/// folder from a runaway command. Past the cap we keep draining the pipes
/// (so the exit code is still captured) but stop writing.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Wall-clock safety ceiling for a single background task. A task that exceeds
/// it is killed and marked `timed_out`. Long enough for typical CI.
const DEFAULT_MAX_RUNTIME_SECS: u64 = 30 * 60;
/// How many tasks to surface in the per-turn system-prompt block.
const DISCLOSED_TASKS: usize = 10;
/// Default tail size returned by `background_output` when the caller does not
/// ask for a specific window.
const DEFAULT_OUTPUT_TAIL_BYTES: usize = 16 * 1024;
/// Cap on concurrently-running background tasks (scripts + sub-agents). A
/// model-facing guardrail against unbounded fan-out — `background_run` /
/// `background_agent` refuse past this until a running task finishes or is
/// cancelled. Generous enough for normal parallel work.
const MAX_CONCURRENT_TASKS: usize = 8;

// ---------- data model ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
    /// A shell command run via `bash -lc` from the workspace root.
    Script,
    /// A sub-agent: a child session that runs one focused turn detached from
    /// the parent turn and returns its final message as the result.
    Agent,
}

impl BackgroundKind {
    fn label(self) -> &'static str {
        match self {
            BackgroundKind::Script => "script",
            BackgroundKind::Agent => "agent",
        }
    }
}

/// Result of running a background sub-agent to completion.
#[derive(Debug)]
pub struct AgentRunResult {
    /// The child session's id — resume its full transcript with `--session <id>`.
    pub session_id: String,
    /// The sub-agent's final assistant message, if any.
    pub final_text: Option<String>,
    /// Whether the child turn completed successfully.
    pub success: bool,
}

/// Builds and drives a child sub-agent session. Implemented in `runtime.rs`
/// (where `build` and the provider live) and injected into the registry, so the
/// background module stays free of runtime-construction details. Absent in
/// child sessions, which is what prevents a sub-agent from spawning its own
/// sub-agents (bounded depth).
#[async_trait]
pub trait AgentSpawner: Send + Sync {
    /// Run a one-shot sub-agent with `prompt` and return its outcome.
    async fn run(&self, prompt: String) -> Result<AgentRunResult, String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    /// Assigned on restore: the task was `running` when a previous yolop exited,
    /// so its OS process did not survive. Not resumable as a process.
    Interrupted,
}

impl BackgroundStatus {
    /// Terminal statuses never transition again.
    fn is_terminal(self) -> bool {
        !matches!(self, BackgroundStatus::Running)
    }

    fn as_str(self) -> &'static str {
        match self {
            BackgroundStatus::Running => "running",
            BackgroundStatus::Completed => "completed",
            BackgroundStatus::Failed => "failed",
            BackgroundStatus::Cancelled => "cancelled",
            BackgroundStatus::TimedOut => "timed_out",
            BackgroundStatus::Interrupted => "interrupted",
        }
    }
}

/// Serialized, restart-survivable description of one background task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackgroundRecord {
    pub id: String,
    pub kind: BackgroundKind,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command: Option<String>,
    pub status: BackgroundStatus,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit_code: Option<i32>,
    /// One-line summary — the last non-empty output line, or a terminal note.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    /// File name (relative to the background dir) holding full output.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub log_file: Option<String>,
    /// For `agent` tasks: the child session id, resumable with `--session <id>`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub child_session_id: Option<String>,
}

impl BackgroundRecord {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind.label(),
            "label": self.label,
            "status": self.status.as_str(),
            "created": self.created.to_rfc3339(),
            "updated": self.updated.to_rfc3339(),
            "exit_code": self.exit_code,
            "summary": self.summary,
            "child_session_id": self.child_session_id,
        })
    }
}

/// On-disk index envelope.
#[derive(Default, Serialize, Deserialize)]
struct IndexFile {
    tasks: Vec<BackgroundRecord>,
}

// ---------- registry ----------

struct Inner {
    records: Vec<BackgroundRecord>,
    /// Live task handles, keyed by id, used to cancel a running task. Finished
    /// handles are pruned in [`BackgroundRegistry::list`] (and removed by
    /// `cancel`), so this stays bounded by the live task count. Not persisted —
    /// handles cannot outlive the process.
    handles: HashMap<String, JoinHandle<()>>,
    /// Task ids whose terminal transition has already been reported to the host
    /// for a proactive wake (see [`BackgroundRegistry::drain_finished_for_wake`]).
    /// Pre-seeded on load with every restored task so historical results don't
    /// trigger a wake on resume — only live transitions this process do.
    notified: HashSet<String>,
}

/// Per-session owner of background tasks. Cheap to clone via the inner `Arc`.
pub struct BackgroundRegistry {
    inner: Arc<Mutex<Inner>>,
    dir: PathBuf,
    index_path: PathBuf,
    workspace_root: PathBuf,
    max_runtime_secs: u64,
    /// Present only when this session may spawn sub-agents (absent in child
    /// sessions — see [`AgentSpawner`]).
    spawner: Option<Arc<dyn AgentSpawner>>,
}

impl BackgroundRegistry {
    /// Open (or restore) the registry for a session. Reads the index if present;
    /// any task still marked `running` is re-labelled `interrupted` (its process
    /// died with the previous yolop) and the corrected index is re-persisted.
    pub fn load(session_dir: &Path, workspace_root: PathBuf) -> Self {
        let dir = session_dir.join(BACKGROUND_SUBDIR);
        let index_path = dir.join(INDEX_FILE);
        let mut records = read_index(&index_path);

        let mut corrected = false;
        let now = Utc::now();
        for record in &mut records {
            if record.status == BackgroundStatus::Running {
                record.status = BackgroundStatus::Interrupted;
                record.updated = now;
                if record.summary.is_none() {
                    record.summary =
                        Some("interrupted: yolop exited while this task was running".into());
                }
                corrected = true;
            }
        }

        // Pre-seed `notified` with every restored task so a resumed session
        // doesn't fire a proactive wake for work that finished before restart;
        // only transitions that happen live this process should wake the host.
        let notified: HashSet<String> = records.iter().map(|r| r.id.clone()).collect();

        let registry = Self {
            inner: Arc::new(Mutex::new(Inner {
                records,
                handles: HashMap::new(),
                notified,
            })),
            dir,
            index_path,
            workspace_root,
            max_runtime_secs: DEFAULT_MAX_RUNTIME_SECS,
            spawner: None,
        };
        if corrected {
            registry.persist();
        }
        registry
    }

    /// Enable sub-agent spawning by attaching the spawner. Top-level sessions
    /// set this; child sessions do not, which bounds sub-agent depth.
    pub fn with_spawner(mut self, spawner: Arc<dyn AgentSpawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Whether this session can spawn sub-agents (the `background_agent` tool is
    /// only offered when true).
    pub fn can_spawn_agents(&self) -> bool {
        self.spawner.is_some()
    }

    #[cfg(test)]
    fn with_max_runtime(mut self, secs: u64) -> Self {
        self.max_runtime_secs = secs;
        self
    }

    /// Snapshot of all tasks, most-recently-updated first. Opportunistically
    /// prunes finished task handles (this is called every turn) so completed
    /// `JoinHandle`s don't accumulate over a long session.
    pub fn list(&self) -> Vec<BackgroundRecord> {
        let mut guard = self.inner.lock().expect("background lock poisoned");
        guard.handles.retain(|_, handle| !handle.is_finished());
        let mut records = guard.records.clone();
        records.sort_by_key(|r| std::cmp::Reverse(r.updated));
        records
    }

    /// Fetch one task by id.
    pub fn get(&self, id: &str) -> Option<BackgroundRecord> {
        let guard = self.inner.lock().expect("background lock poisoned");
        guard.records.iter().find(|r| r.id == id).cloned()
    }

    /// Cheap `(running, total)` task counts for the status bar — avoids cloning
    /// the whole record set every render frame. The TUI polls this every frame,
    /// so it also prunes finished handles here (as `list()` does) to keep the
    /// handle map bounded even on turns where `list()` is never called.
    pub fn counts(&self) -> (usize, usize) {
        let mut guard = self.inner.lock().expect("background lock poisoned");
        guard.handles.retain(|_, handle| !handle.is_finished());
        let running = guard
            .records
            .iter()
            .filter(|r| r.status == BackgroundStatus::Running)
            .count();
        (running, guard.records.len())
    }

    /// Return tasks that have reached a terminal status since the last call and
    /// have not yet been reported for a proactive wake, marking them reported.
    /// The host (TUI) polls this while idle and, when non-empty, wakes the agent
    /// so it can react to finished background work without a user prompt. Tasks
    /// restored from a previous run are pre-marked, so only live transitions
    /// this process trigger a wake.
    pub fn drain_finished_for_wake(&self) -> Vec<BackgroundRecord> {
        let mut guard = self.inner.lock().expect("background lock poisoned");
        let finished: Vec<BackgroundRecord> = guard
            .records
            .iter()
            .filter(|r| r.status.is_terminal() && !guard.notified.contains(&r.id))
            .cloned()
            .collect();
        for r in &finished {
            guard.notified.insert(r.id.clone());
        }
        finished
    }

    /// `Some(error)` when too many tasks are already running, for the spawn
    /// tools to reject new work. `None` when there is capacity.
    pub fn capacity_error(&self) -> Option<String> {
        let (running, _) = self.counts();
        (running >= MAX_CONCURRENT_TASKS).then(|| {
            format!(
                "too many background tasks already running ({running}/{MAX_CONCURRENT_TASKS}); \
                 wait for one to finish or cancel one with `background_cancel`"
            )
        })
    }

    /// Human-readable task list for the `/background` command, most-recently-
    /// updated first (same ordering as `list`).
    pub fn render_task_list(&self) -> String {
        let records = self.list();
        if records.is_empty() {
            return "No background tasks in this session.".to_string();
        }
        let (running, total) = (
            records
                .iter()
                .filter(|r| r.status == BackgroundStatus::Running)
                .count(),
            records.len(),
        );
        let mut out = format!("{total} background task(s), {running} running:\n");
        for r in &records {
            let exit = r
                .exit_code
                .map(|c| format!(" exit={c}"))
                .unwrap_or_default();
            let summary = r
                .summary
                .as_deref()
                .map(|s| format!(" — {s}"))
                .unwrap_or_default();
            let child = r
                .child_session_id
                .as_deref()
                .map(|s| format!(" (session {s})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  [{id}] {kind} {status}{exit}: {label}{summary}{child}\n",
                id = r.id,
                kind = r.kind.label(),
                status = r.status.as_str(),
                label = r.label,
            ));
        }
        out.push_str("\nRead full output with the background_output tool.");
        out
    }

    /// Start a scripted background task. Returns the new record immediately; the
    /// command runs on a detached task and updates its record as it progresses.
    pub fn spawn_script(&self, label: Option<String>, command: String) -> BackgroundRecord {
        let now = Utc::now();
        let id = self.next_id();
        let log_file = format!("{id}.log");
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| first_line(&command, 60));
        let record = BackgroundRecord {
            id: id.clone(),
            kind: BackgroundKind::Script,
            label,
            command: Some(command.clone()),
            status: BackgroundStatus::Running,
            created: now,
            updated: now,
            exit_code: None,
            summary: None,
            log_file: Some(log_file.clone()),
            child_session_id: None,
        };

        {
            let mut guard = self.inner.lock().expect("background lock poisoned");
            guard.records.push(record.clone());
        }
        self.persist();

        let inner = self.inner.clone();
        let index_path = self.index_path.clone();
        let dir = self.dir.clone();
        let workspace_root = self.workspace_root.clone();
        let max_runtime = self.max_runtime_secs;
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            let outcome = run_script(&dir, &log_file, &workspace_root, &command, max_runtime).await;
            update_record(&inner, &index_path, &task_id, |r| {
                // Only apply the outcome if the task is still running. A
                // concurrent `cancel` may have already marked it `cancelled`
                // after the child exited but before this update ran; do not
                // clobber that terminal status.
                if r.status != BackgroundStatus::Running {
                    return false;
                }
                r.status = outcome.status;
                r.exit_code = outcome.exit_code;
                r.summary = Some(outcome.summary.clone());
                true
            });
        });

        let mut guard = self.inner.lock().expect("background lock poisoned");
        guard.handles.insert(id, handle);
        record
    }

    /// Start a background sub-agent: a child session that runs one focused turn
    /// with `task` and returns its final message. Returns the new record
    /// immediately; the agent runs detached. Errors if this session can't spawn
    /// sub-agents (no spawner — e.g. inside another sub-agent).
    pub fn spawn_agent(
        &self,
        label: Option<String>,
        task: String,
    ) -> Result<BackgroundRecord, String> {
        let spawner = self
            .spawner
            .clone()
            .ok_or_else(|| "background sub-agents are not available in this session".to_string())?;

        let now = Utc::now();
        let id = self.next_id();
        let log_file = format!("{id}.log");
        let label = label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| first_line(&task, 60));
        let record = BackgroundRecord {
            id: id.clone(),
            kind: BackgroundKind::Agent,
            label,
            command: Some(task.clone()),
            status: BackgroundStatus::Running,
            created: now,
            updated: now,
            exit_code: None,
            summary: None,
            log_file: Some(log_file.clone()),
            child_session_id: None,
        };

        {
            let mut guard = self.inner.lock().expect("background lock poisoned");
            guard.records.push(record.clone());
        }
        self.persist();

        let inner = self.inner.clone();
        let index_path = self.index_path.clone();
        let dir = self.dir.clone();
        let max_runtime = self.max_runtime_secs;
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            let outcome = run_agent(spawner, &dir, &log_file, task, max_runtime).await;
            update_record(&inner, &index_path, &task_id, |r| {
                // Don't clobber a status a concurrent `cancel` already set.
                if r.status != BackgroundStatus::Running {
                    return false;
                }
                r.status = outcome.status;
                r.summary = Some(outcome.summary.clone());
                r.child_session_id = outcome.child_session_id.clone();
                true
            });
        });

        let mut guard = self.inner.lock().expect("background lock poisoned");
        guard.handles.insert(id, handle);
        Ok(record)
    }

    /// Read a task's captured output (the tail of its log), capped at `max_bytes`.
    pub fn read_output(
        &self,
        id: &str,
        max_bytes: usize,
    ) -> Option<(BackgroundRecord, String, bool)> {
        let record = self.get(id)?;
        let log = record.log_file.as_ref()?;
        let path = self.dir.join(log);
        let bytes = std::fs::read(&path).unwrap_or_default();
        let truncated = bytes.len() > max_bytes;
        let slice = if truncated {
            &bytes[bytes.len() - max_bytes..]
        } else {
            &bytes[..]
        };
        let text = String::from_utf8_lossy(slice).to_string();
        Some((record, text, truncated))
    }

    /// Cancel a running task. Aborts its detached task (whose child is reaped via
    /// `kill_on_drop`) and marks it `cancelled`. Returns true if it was running.
    pub fn cancel(&self, id: &str) -> bool {
        let mut guard = self.inner.lock().expect("background lock poisoned");
        if let Some(handle) = guard.handles.remove(id) {
            handle.abort();
        }
        let now = Utc::now();
        let mut changed = false;
        if let Some(record) = guard.records.iter_mut().find(|r| r.id == id)
            && record.status == BackgroundStatus::Running
        {
            record.status = BackgroundStatus::Cancelled;
            record.updated = now;
            record.summary = Some("cancelled".into());
            changed = true;
        }
        drop(guard);
        if changed {
            self.persist();
        }
        changed
    }

    /// Render the per-turn `<background_tasks>` block, or `None` when there are
    /// no tasks. Disclosed newest-first and capped at [`DISCLOSED_TASKS`].
    fn system_prompt_block(&self) -> Option<String> {
        let records = self.list();
        if records.is_empty() {
            return None;
        }
        let total = records.len();
        let running = records
            .iter()
            .filter(|r| r.status == BackgroundStatus::Running)
            .count();

        let mut out = String::from("<background_tasks>\n");
        out.push_str(
            "Background tasks run detached from this turn. Start one with `background_run`, \
             list with `background_list`, read a task's output (most recent tail) with \
             `background_output`, cancel with `background_cancel`. A `completed`/`failed` task's \
             result is ready to read NOW.\n",
        );
        if self.can_spawn_agents() {
            out.push_str(
                "Spin off a focused sub-agent with `background_agent` for a self-contained piece \
                 of work (analysis, drafting); read its result the same way.\n",
            );
        }
        out.push_str(&format!(
            "{total} task(s), {running} running (most recent first):\n"
        ));
        for r in records.iter().take(DISCLOSED_TASKS) {
            let summary = r
                .summary
                .as_deref()
                .map(|s| format!(" — {s}"))
                .unwrap_or_default();
            let exit = r
                .exit_code
                .map(|c| format!(" exit={c}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- [{id}] {status}{exit}: {label}{summary}\n",
                id = r.id,
                status = r.status.as_str(),
                label = r.label,
            ));
        }
        if total > DISCLOSED_TASKS {
            out.push_str("(more tasks exist — use `background_list` to see them.)\n");
        }
        out.push_str("</background_tasks>");
        Some(out)
    }

    /// Generate a short id unique within the current task set.
    fn next_id(&self) -> String {
        let guard = self.inner.lock().expect("background lock poisoned");
        loop {
            let id = format!("bg-{:06x}", rand::random::<u32>() & 0xFF_FFFF);
            if !guard.records.iter().any(|r| r.id == id) {
                return id;
            }
        }
    }

    /// Atomically write the index. Best-effort: a persistence failure is logged
    /// but never sinks a running task.
    fn persist(&self) {
        let records = {
            let guard = self.inner.lock().expect("background lock poisoned");
            guard.records.clone()
        };
        if let Err(e) = write_index(&self.index_path, &records) {
            tracing::warn!(path = %self.index_path.display(), error = %e, "failed to persist background index");
        }
    }
}

/// Mutate one record under the lock and, only when the mutator reports a change
/// (returns `true`), stamp `updated` and persist. The bool guard lets a caller
/// no-op when the record is no longer in the expected state (e.g. it was
/// cancelled out from under a finishing task) without a redundant write.
fn update_record(
    inner: &Arc<Mutex<Inner>>,
    index_path: &Path,
    id: &str,
    f: impl FnOnce(&mut BackgroundRecord) -> bool,
) {
    let records = {
        let mut guard = inner.lock().expect("background lock poisoned");
        let changed = match guard.records.iter_mut().find(|r| r.id == id) {
            Some(record) => {
                let changed = f(record);
                if changed {
                    record.updated = Utc::now();
                }
                changed
            }
            None => false,
        };
        // Record missing, or the mutator made no change — nothing to persist.
        if !changed {
            return;
        }
        guard.records.clone()
    };
    if let Err(e) = write_index(index_path, &records) {
        tracing::warn!(path = %index_path.display(), error = %e, "failed to persist background index");
    }
}

/// Outcome of a finished scripted task.
struct ScriptOutcome {
    status: BackgroundStatus,
    exit_code: Option<i32>,
    summary: String,
}

/// Run a shell command in the background, streaming output to `<dir>/<log_file>`
/// as it arrives. Returns the terminal status, exit code, and a one-line summary.
async fn run_script(
    dir: &Path,
    log_file: &str,
    workspace_root: &Path,
    command: &str,
    max_runtime_secs: u64,
) -> ScriptOutcome {
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        return ScriptOutcome {
            status: BackgroundStatus::Failed,
            exit_code: None,
            summary: format!("could not create background dir: {e}"),
        };
    }
    let log_path = dir.join(log_file);
    let mut log = match tokio::fs::File::create(&log_path).await {
        Ok(f) => f,
        Err(e) => {
            return ScriptOutcome {
                status: BackgroundStatus::Failed,
                exit_code: None,
                summary: format!("could not open log file: {e}"),
            };
        }
    };
    // Owner-only: the log can echo workspace contents, so keep it as private as
    // the session JSONL and the background index. `File::create` would leave the
    // OS default (often 0o644).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600)).await;
    }

    let mut child = match Command::new("bash")
        .arg("-lc")
        .arg(command)
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ScriptOutcome {
                status: BackgroundStatus::Failed,
                exit_code: None,
                summary: format!("spawn failed: {e}"),
            };
        }
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let drive = async {
        // `current` is the line being accumulated; `last_complete` is the most
        // recent finished non-empty line. The summary is whichever holds the
        // last non-empty content when the streams close.
        let mut current = String::new();
        let mut last_complete = String::new();
        let mut written = 0usize;
        let mut o = vec![0u8; 8192];
        let mut e = vec![0u8; 8192];
        let mut out_done = false;
        let mut err_done = false;
        while !(out_done && err_done) {
            let chunk = tokio::select! {
                biased;
                n = stdout.read(&mut o), if !out_done => match n {
                    Ok(0) | Err(_) => { out_done = true; None }
                    Ok(n) => Some(o[..n].to_vec()),
                },
                n = stderr.read(&mut e), if !err_done => match n {
                    Ok(0) | Err(_) => { err_done = true; None }
                    Ok(n) => Some(e[..n].to_vec()),
                },
            };
            if let Some(chunk) = chunk {
                for ch in String::from_utf8_lossy(&chunk).chars() {
                    match ch {
                        '\n' => {
                            if !current.trim().is_empty() {
                                last_complete = std::mem::take(&mut current);
                            } else {
                                current.clear();
                            }
                        }
                        '\r' => {}
                        _ => current.push(ch),
                    }
                }
                if written < MAX_OUTPUT_BYTES {
                    let room = MAX_OUTPUT_BYTES - written;
                    let slice = &chunk[..chunk.len().min(room)];
                    let _ = log.write_all(slice).await;
                    written += slice.len();
                    if written >= MAX_OUTPUT_BYTES {
                        let _ = log
                            .write_all(b"\n[background: output truncated at 256 KiB]\n")
                            .await;
                    }
                }
            }
        }
        let _ = log.flush().await;
        let status = child.wait().await;
        let last_line = if current.trim().is_empty() {
            last_complete
        } else {
            current
        };
        (status, last_line)
    };

    let timeout = std::time::Duration::from_secs(max_runtime_secs);
    match tokio::time::timeout(timeout, drive).await {
        Ok((status, last_line)) => {
            let exit_code = status.as_ref().ok().and_then(|s| s.code());
            let success = matches!(&status, Ok(s) if s.success());
            let summary = summarize(&last_line, exit_code, success);
            ScriptOutcome {
                status: if success {
                    BackgroundStatus::Completed
                } else {
                    BackgroundStatus::Failed
                },
                exit_code,
                summary,
            }
        }
        Err(_) => {
            // `drive` is dropped here; the owned child is dropped with it and
            // `kill_on_drop` reaps the OS process.
            ScriptOutcome {
                status: BackgroundStatus::TimedOut,
                exit_code: None,
                summary: format!("timed out after {max_runtime_secs}s"),
            }
        }
    }
}

/// Terminal state of a finished sub-agent task.
struct AgentOutcome {
    status: BackgroundStatus,
    summary: String,
    child_session_id: Option<String>,
}

/// Drive a background sub-agent to completion: run the child turn via the
/// spawner under a wall-clock ceiling, write its final message to the task log,
/// and return the terminal state. Mirrors `run_script`'s timeout/cap discipline.
async fn run_agent(
    spawner: Arc<dyn AgentSpawner>,
    dir: &Path,
    log_file: &str,
    task: String,
    max_runtime_secs: u64,
) -> AgentOutcome {
    let timeout = std::time::Duration::from_secs(max_runtime_secs);
    match tokio::time::timeout(timeout, spawner.run(task)).await {
        // Timed out: the spawner future (and the child turn it owns) is dropped,
        // abandoning the run. Match `run_script`'s `timed_out` semantics.
        Err(_) => {
            write_log(
                dir,
                log_file,
                &format!("sub-agent timed out after {max_runtime_secs}s\n"),
            )
            .await;
            AgentOutcome {
                status: BackgroundStatus::TimedOut,
                summary: format!("timed out after {max_runtime_secs}s"),
                child_session_id: None,
            }
        }
        Ok(Ok(run)) => {
            let final_text = run.final_text.unwrap_or_default();
            let shown = if final_text.trim().is_empty() {
                "(sub-agent produced no final message)"
            } else {
                &final_text
            };
            // Header AND footer carry the child session id, so it stays visible
            // even when `background_output` returns only the tail of a long log.
            let body = format!(
                "background sub-agent\nchild session: {sid}\nsuccess: {ok}\n\n{shown}\n\n\
                 [child session: {sid}]\n",
                sid = run.session_id,
                ok = run.success,
            );
            write_log(dir, log_file, &body).await;
            let summary = if final_text.trim().is_empty() {
                if run.success {
                    "sub-agent finished".to_string()
                } else {
                    "sub-agent failed".to_string()
                }
            } else {
                first_line(&final_text, 200)
            };
            AgentOutcome {
                status: if run.success {
                    BackgroundStatus::Completed
                } else {
                    BackgroundStatus::Failed
                },
                summary,
                child_session_id: Some(run.session_id),
            }
        }
        Ok(Err(e)) => {
            write_log(dir, log_file, &format!("sub-agent failed to run: {e}\n")).await;
            AgentOutcome {
                status: BackgroundStatus::Failed,
                summary: truncate(&format!("error: {e}"), 200),
                child_session_id: None,
            }
        }
    }
}

/// Write a complete log file for a task (used by sub-agents, which produce their
/// output all at once). Capped at [`MAX_OUTPUT_BYTES`] like the script log so a
/// verbose sub-agent can't bloat the session folder; owner-only on Unix.
async fn write_log(dir: &Path, log_file: &str, body: &str) {
    if tokio::fs::create_dir_all(dir).await.is_err() {
        return;
    }
    let capped = cap_bytes(body, MAX_OUTPUT_BYTES);
    let path = dir.join(log_file);
    if tokio::fs::write(&path, capped.as_ref()).await.is_err() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await;
    }
}

/// Truncate `s` to at most `max` bytes on a char boundary, appending a note when
/// truncation happens. Borrows when no truncation is needed.
fn cap_bytes(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    if s.len() <= max {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!(
        "{}\n[background: output truncated at {} KiB]\n",
        &s[..end],
        max / 1024
    ))
}

fn summarize(last_line: &str, exit_code: Option<i32>, success: bool) -> String {
    let tail = last_line.trim();
    if !tail.is_empty() {
        return truncate(tail, 200);
    }
    match exit_code {
        Some(code) if success => format!("completed (exit {code})"),
        Some(code) => format!("failed (exit {code})"),
        None => "finished".to_string(),
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    truncate(line, max)
}

/// One-line description of a finished task for the wake prompt.
fn wake_line(t: &BackgroundRecord) -> String {
    let exit = t
        .exit_code
        .map(|c| format!(", exit {c}"))
        .unwrap_or_default();
    let summary = t.summary.as_deref().unwrap_or("(no summary)");
    format!(
        "[{id}] {kind} {status}{exit} — {summary}",
        id = t.id,
        kind = t.kind.label(),
        status = t.status.as_str(),
    )
}

/// Compose the synthetic turn prompt used to proactively wake the agent when
/// background work finishes. Phrased as an automatic notification (not a user
/// message) and points the model at `background_output` for full results.
pub(crate) fn wake_prompt(finished: &[BackgroundRecord]) -> String {
    if let [t] = finished {
        format!(
            "[automatic] A background task you started has finished: {line}. This is not a user \
             message. Read its full output with `background_output` (id {id}) if useful, then \
             continue the work it was for or report the result.",
            line = wake_line(t),
            id = t.id,
        )
    } else {
        let mut out = format!(
            "[automatic] {} background tasks you started have finished:\n",
            finished.len()
        );
        for t in finished {
            out.push_str("- ");
            out.push_str(&wake_line(t));
            out.push('\n');
        }
        out.push_str(
            "This is not a user message. Use `background_output` to read any result, then continue \
             the work or report back.",
        );
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------- persistence ----------

/// Process-wide counter for unique index staging-file names. See `write_index`.
static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn read_index(path: &Path) -> Vec<BackgroundRecord> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<IndexFile>(&bytes) {
        Ok(index) => index.tasks,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "ignoring malformed background index");
            Vec::new()
        }
    }
}

/// Atomic write (temp file + rename), owner-only on Unix — background output can
/// echo workspace contents, so the index stays private like the session log.
fn write_index(path: &Path, records: &[BackgroundRecord]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let index = IndexFile {
        tasks: records.to_vec(),
    };
    let bytes = serde_json::to_vec_pretty(&index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Per-write unique temp name: tasks finish concurrently and each persists
    // outside the lock, so a pid-only temp path would let two writes collide on
    // the same file. A process-wide counter keeps each staging file distinct;
    // the atomic rename then makes the last consistent snapshot win.
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".{INDEX_FILE}.tmp.{}.{seq}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    // Clean up the staging file if any step fails, so a write error never leaves
    // a stray temp behind.
    let staged = (|| -> std::io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)
}

// ---------- capability ----------

pub(crate) struct BackgroundCapability {
    pub(crate) registry: Arc<BackgroundRegistry>,
}

#[async_trait]
impl Capability for BackgroundCapability {
    fn id(&self) -> &str {
        BACKGROUND_CAPABILITY_ID
    }
    fn name(&self) -> &str {
        "Background execution"
    }
    fn description(&self) -> &str {
        "Run shell commands detached from the current turn (e.g. waiting for CI), then read their \
         results on a later turn. Tasks survive a restart's results via the per-session folder."
    }
    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }
    fn category(&self) -> Option<&str> {
        Some("Execution")
    }

    fn commands(&self) -> Vec<CommandDescriptor> {
        vec![CommandDescriptor {
            name: "background".to_string(),
            description: "list background tasks and their status".to_string(),
            source: CommandSource::System,
            args: Vec::new(),
        }]
    }

    async fn execute_command(
        &self,
        request: &ExecuteCommandRequest,
        _ctx: &CommandExecutionContext,
    ) -> everruns_core::Result<CommandResult> {
        if request.name != "background" {
            return Err(everruns_core::AgentLoopError::config(format!(
                "{} cannot execute /{}",
                self.id(),
                request.name
            )));
        }
        Ok(CommandResult {
            success: true,
            message: self.registry.render_task_list(),
            error_code: None,
            error_fields: None,
        })
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        self.registry.system_prompt_block()
    }

    fn system_prompt_preview(&self) -> Option<String> {
        Some(
            "\
<background_tasks>
Background tasks run detached from this turn (background_run / background_list /
background_output / background_cancel).
1 task(s), 1 running (most recent first):
- [bg-1a2b3c] running: wait for CI — Run #42 in progress
</background_tasks>"
                .to_string(),
        )
    }

    fn tools(&self) -> Vec<Box<dyn Tool>> {
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(BackgroundRunTool {
                registry: self.registry.clone(),
            }),
            Box::new(BackgroundListTool {
                registry: self.registry.clone(),
            }),
            Box::new(BackgroundOutputTool {
                registry: self.registry.clone(),
            }),
            Box::new(BackgroundCancelTool {
                registry: self.registry.clone(),
            }),
        ];
        // The sub-agent tool is only offered when this session can spawn agents
        // (i.e. it is not itself a sub-agent) — this bounds sub-agent depth.
        if self.registry.can_spawn_agents() {
            tools.push(Box::new(BackgroundAgentTool {
                registry: self.registry.clone(),
            }));
        }
        tools
    }
}

// ---------- tools ----------

struct BackgroundRunTool {
    registry: Arc<BackgroundRegistry>,
}

#[async_trait]
impl Tool for BackgroundRunTool {
    fn name(&self) -> &str {
        "background_run"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Run in background")
    }
    fn description(&self) -> &str {
        "Start a shell command that runs DETACHED from this turn and returns immediately. Use for \
         long waits that should not block you — most commonly waiting for CI (e.g. \
         `gh pr checks <pr> --watch`). Returns a task id; read its result later with \
         `background_output` (its status also shows up at the top of later turns). Do NOT use this \
         for quick commands — use `bash` for those."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to run via `bash -lc` from the workspace root."
                },
                "label": {
                    "type": "string",
                    "description": "Optional short human label (e.g. \"wait for CI on PR 42\"). Defaults to the command."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let command = match arguments.get("command").and_then(Value::as_str) {
            Some(c) if !c.trim().is_empty() => c.to_string(),
            _ => {
                return ToolExecutionResult::tool_error(
                    "'command' is required and must be non-empty",
                );
            }
        };
        if let Some(err) = self.registry.capacity_error() {
            return ToolExecutionResult::tool_error(err);
        }
        let label = arguments
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_string);
        let record = self.registry.spawn_script(label, command);
        ToolExecutionResult::success(json!({
            "ok": true,
            "id": record.id,
            "status": record.status.as_str(),
            "label": record.label,
            "message": format!(
                "started background task {} — check `background_output` later or watch later turns.",
                record.id
            ),
        }))
    }
}

struct BackgroundAgentTool {
    registry: Arc<BackgroundRegistry>,
}

#[async_trait]
impl Tool for BackgroundAgentTool {
    fn name(&self) -> &str {
        "background_agent"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Run sub-agent in background")
    }
    fn description(&self) -> &str {
        "Spin off a focused sub-agent that runs DETACHED from this turn in its own session, with \
         the same tools and workspace, and returns its final message. Use to parallelize a \
         self-contained piece of work whose intermediate output would otherwise flood your context \
         (e.g. \"analyze the auth module and summarize the risks\", \"draft an integration test for \
         X\"). Give it a complete, standalone instruction — it does not see this conversation. \
         Returns a task id; read its result later with `background_output` (status also shows at \
         the top of later turns). The sub-agent cannot spawn further sub-agents."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Complete, standalone instruction for the sub-agent. It does not see this conversation, so include all needed context."
                },
                "label": {
                    "type": "string",
                    "description": "Optional short human label (e.g. \"analyze auth module\"). Defaults to the task."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let task = match arguments.get("task").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => {
                return ToolExecutionResult::tool_error("'task' is required and must be non-empty");
            }
        };
        if let Some(err) = self.registry.capacity_error() {
            return ToolExecutionResult::tool_error(err);
        }
        let label = arguments
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_string);
        match self.registry.spawn_agent(label, task) {
            Ok(record) => ToolExecutionResult::success(json!({
                "ok": true,
                "id": record.id,
                "status": record.status.as_str(),
                "label": record.label,
                "message": format!(
                    "started background sub-agent {} — read its result with `background_output` or watch later turns.",
                    record.id
                ),
            })),
            Err(e) => ToolExecutionResult::tool_error(e),
        }
    }
}

struct BackgroundListTool {
    registry: Arc<BackgroundRegistry>,
}

#[async_trait]
impl Tool for BackgroundListTool {
    fn name(&self) -> &str {
        "background_list"
    }
    fn display_name(&self) -> Option<&str> {
        Some("List background tasks")
    }
    fn description(&self) -> &str {
        "List background tasks for this session with their status and one-line summary."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }
    async fn execute(&self, _arguments: Value) -> ToolExecutionResult {
        let records = self.registry.list();
        ToolExecutionResult::success(json!({
            "ok": true,
            "count": records.len(),
            "tasks": records.iter().map(BackgroundRecord::to_json).collect::<Vec<_>>(),
        }))
    }
}

struct BackgroundOutputTool {
    registry: Arc<BackgroundRegistry>,
}

#[async_trait]
impl Tool for BackgroundOutputTool {
    fn name(&self) -> &str {
        "background_output"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Read background output")
    }
    fn description(&self) -> &str {
        "Read a background task's captured output (the tail of its log) by id, along with its \
         status and exit code. Works while the task is still running (partial output) or after it \
         finished."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The background task id (from `background_run`/`background_list`)." },
                "max_bytes": { "type": "integer", "minimum": 1, "description": "Max bytes of output tail to return (default 16384)." }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let id = match arguments.get("id").and_then(Value::as_str) {
            Some(id) if !id.trim().is_empty() => id.trim(),
            _ => return ToolExecutionResult::tool_error("'id' is required and must be non-empty"),
        };
        let max_bytes = arguments
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_OUTPUT_TAIL_BYTES);
        match self.registry.read_output(id, max_bytes) {
            Some((record, output, truncated)) => ToolExecutionResult::success(json!({
                "ok": true,
                "id": record.id,
                "status": record.status.as_str(),
                "exit_code": record.exit_code,
                "summary": record.summary,
                // Reported structurally so a sub-agent's child session id is
                // always available even if the log tail truncated the header.
                "child_session_id": record.child_session_id,
                "output": output,
                "truncated": truncated,
            })),
            None => ToolExecutionResult::success(json!({
                "ok": true,
                "id": id,
                "found": false,
                "message": format!("no background task with id '{id}'"),
            })),
        }
    }
}

struct BackgroundCancelTool {
    registry: Arc<BackgroundRegistry>,
}

#[async_trait]
impl Tool for BackgroundCancelTool {
    fn name(&self) -> &str {
        "background_cancel"
    }
    fn display_name(&self) -> Option<&str> {
        Some("Cancel background task")
    }
    fn description(&self) -> &str {
        "Cancel a running background task by id. Already-finished tasks are left as-is."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The background task id to cancel." }
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, arguments: Value) -> ToolExecutionResult {
        let id = match arguments.get("id").and_then(Value::as_str) {
            Some(id) if !id.trim().is_empty() => id.trim(),
            _ => return ToolExecutionResult::tool_error("'id' is required and must be non-empty"),
        };
        let cancelled = self.registry.cancel(id);
        let status = self.registry.get(id).map(|r| r.status.as_str().to_string());
        ToolExecutionResult::success(json!({
            "ok": true,
            "id": id,
            "cancelled": cancelled,
            "status": status,
            "message": if cancelled {
                format!("cancelled background task {id}")
            } else {
                format!("background task {id} was not running (already finished or unknown)")
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_in(dir: &Path) -> BackgroundRegistry {
        BackgroundRegistry::load(dir, dir.to_path_buf())
    }

    /// Test spawner that returns a canned outcome, so the registry's sub-agent
    /// bookkeeping can be tested without building a real child runtime.
    struct MockSpawner {
        final_text: Option<String>,
        success: bool,
    }

    #[async_trait]
    impl AgentSpawner for MockSpawner {
        async fn run(&self, _prompt: String) -> Result<AgentRunResult, String> {
            Ok(AgentRunResult {
                session_id: "session_child_test".to_string(),
                final_text: self.final_text.clone(),
                success: self.success,
            })
        }
    }

    /// Spawner that never finishes in time, to exercise the agent timeout path.
    struct SlowSpawner;

    #[async_trait]
    impl AgentSpawner for SlowSpawner {
        async fn run(&self, _prompt: String) -> Result<AgentRunResult, String> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            unreachable!("should be cancelled by the timeout")
        }
    }

    /// Poll until a task reaches a terminal status, or panic after `tries`.
    async fn wait_terminal(reg: &BackgroundRegistry, id: &str, tries: u32) -> BackgroundRecord {
        for _ in 0..tries {
            if let Some(r) = reg.get(id)
                && r.status.is_terminal()
            {
                return r;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("task {id} did not reach a terminal status in time");
    }

    #[tokio::test]
    async fn script_completes_and_captures_output() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let record = reg.spawn_script(Some("hello task".into()), "echo hello-bg".into());
        assert_eq!(record.status, BackgroundStatus::Running);

        let done = wait_terminal(&reg, &record.id, 100).await;
        assert_eq!(done.status, BackgroundStatus::Completed);
        assert_eq!(done.exit_code, Some(0));

        let (_, output, _) = reg.read_output(&record.id, 64 * 1024).unwrap();
        assert!(output.contains("hello-bg"), "got: {output}");
    }

    #[tokio::test]
    async fn nonzero_exit_is_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let record = reg.spawn_script(None, "echo boom 1>&2; exit 3".into());
        let done = wait_terminal(&reg, &record.id, 100).await;
        assert_eq!(done.status, BackgroundStatus::Failed);
        assert_eq!(done.exit_code, Some(3));
        let (_, output, _) = reg.read_output(&record.id, 64 * 1024).unwrap();
        assert!(output.contains("boom"), "got: {output}");
    }

    #[tokio::test]
    async fn cancel_marks_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        let record = reg.spawn_script(None, "sleep 30".into());
        // Give the child a moment to actually start.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(reg.cancel(&record.id));
        let got = reg.get(&record.id).unwrap();
        assert_eq!(got.status, BackgroundStatus::Cancelled);
        // Cancelling again is a no-op (already terminal).
        assert!(!reg.cancel(&record.id));
        // The aborted task must not race back and clobber `cancelled` with a
        // terminal script outcome — give it time to (not) do so.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            reg.get(&record.id).unwrap().status,
            BackgroundStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn timeout_marks_timed_out() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path()).with_max_runtime(1);
        let record = reg.spawn_script(None, "sleep 30".into());
        let done = wait_terminal(&reg, &record.id, 100).await;
        assert_eq!(done.status, BackgroundStatus::TimedOut);
    }

    #[tokio::test]
    async fn results_survive_restart_and_running_becomes_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        // First "process": run a task to completion.
        let first_id = {
            let reg = registry_in(tmp.path());
            let record = reg.spawn_script(Some("done task".into()), "echo persisted".into());
            wait_terminal(&reg, &record.id, 100).await;
            record.id
        };

        // Inject a stale `running` record straight into the index to simulate a
        // task that was mid-flight when the previous process died.
        let index_path = tmp.path().join(BACKGROUND_SUBDIR).join(INDEX_FILE);
        let mut tasks = read_index(&index_path);
        let now = Utc::now();
        tasks.push(BackgroundRecord {
            id: "bg-stale1".into(),
            kind: BackgroundKind::Script,
            label: "stuck".into(),
            command: Some("sleep 999".into()),
            status: BackgroundStatus::Running,
            created: now,
            updated: now,
            exit_code: None,
            summary: None,
            log_file: None,
            child_session_id: None,
        });
        write_index(&index_path, &tasks).unwrap();

        // Second "process": restore.
        let reg = registry_in(tmp.path());
        let completed = reg.get(&first_id).expect("completed task survives restart");
        assert_eq!(completed.status, BackgroundStatus::Completed);
        let stale = reg.get("bg-stale1").expect("stale task restored");
        assert_eq!(stale.status, BackgroundStatus::Interrupted);
        assert!(
            stale
                .summary
                .as_deref()
                .unwrap_or("")
                .contains("interrupted")
        );
    }

    #[tokio::test]
    async fn capability_exposes_four_tools_and_discloses_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Arc::new(registry_in(tmp.path()));
        let record = reg.spawn_script(Some("ci".into()), "echo hi".into());
        wait_terminal(&reg, &record.id, 100).await;

        let cap = BackgroundCapability {
            registry: reg.clone(),
        };
        let names: Vec<String> = cap.tools().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(
            names,
            vec![
                "background_run",
                "background_list",
                "background_output",
                "background_cancel"
            ]
        );

        let block = reg.system_prompt_block().expect("block present with tasks");
        assert!(block.contains("<background_tasks>"));
        assert!(block.contains(&record.id));
    }

    #[test]
    fn no_tasks_means_no_prompt_block() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        assert!(reg.system_prompt_block().is_none());
    }

    #[tokio::test]
    async fn counts_and_render_task_list_reflect_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        assert_eq!(reg.counts(), (0, 0));
        assert!(reg.render_task_list().contains("No background tasks"));

        let record = reg.spawn_script(Some("ci".into()), "echo hi".into());
        wait_terminal(&reg, &record.id, 100).await;

        let (running, total) = reg.counts();
        assert_eq!((running, total), (0, 1));
        let listed = reg.render_task_list();
        assert!(listed.contains(&record.id), "got: {listed}");
        assert!(listed.contains("script"), "got: {listed}");
        assert!(listed.contains("completed"), "got: {listed}");
    }

    #[tokio::test]
    async fn drain_finished_for_wake_returns_each_task_once() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        assert!(reg.drain_finished_for_wake().is_empty());

        let record = reg.spawn_script(None, "echo hi".into());
        wait_terminal(&reg, &record.id, 100).await;

        let first = reg.drain_finished_for_wake();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, record.id);
        // A given terminal task only wakes once.
        assert!(reg.drain_finished_for_wake().is_empty());
    }

    #[tokio::test]
    async fn restored_terminal_tasks_do_not_wake_on_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let id = {
            let reg = registry_in(tmp.path());
            let record = reg.spawn_script(None, "echo hi".into());
            wait_terminal(&reg, &record.id, 100).await;
            record.id
        };
        // Fresh registry over the same folder = a restart.
        let reg = registry_in(tmp.path());
        assert!(reg.get(&id).is_some(), "result survives restart");
        assert!(
            reg.drain_finished_for_wake().is_empty(),
            "historical results must not trigger a proactive wake on resume"
        );
    }

    #[test]
    fn wake_prompt_describes_single_and_multiple_tasks() {
        let now = Utc::now();
        let rec = |id: &str, kind: BackgroundKind| BackgroundRecord {
            id: id.to_string(),
            kind,
            label: "x".into(),
            command: None,
            status: BackgroundStatus::Completed,
            created: now,
            updated: now,
            exit_code: Some(0),
            summary: Some("done".into()),
            log_file: None,
            child_session_id: None,
        };
        let one = wake_prompt(&[rec("bg-1", BackgroundKind::Script)]);
        assert!(one.contains("bg-1"), "got: {one}");
        assert!(one.contains("automatic"), "got: {one}");
        assert!(one.contains("background_output"), "got: {one}");

        let many = wake_prompt(&[
            rec("bg-1", BackgroundKind::Script),
            rec("bg-2", BackgroundKind::Agent),
        ]);
        assert!(
            many.contains("bg-1") && many.contains("bg-2"),
            "got: {many}"
        );
        assert!(many.contains("2 background tasks"), "got: {many}");
    }

    #[tokio::test]
    async fn capacity_error_blocks_spawn_at_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Arc::new(registry_in(tmp.path()));
        assert!(reg.capacity_error().is_none());

        // Fill to the cap with long-running scripts (Running synchronously).
        for _ in 0..MAX_CONCURRENT_TASKS {
            reg.spawn_script(None, "sleep 30".into());
        }
        assert_eq!(reg.counts().0, MAX_CONCURRENT_TASKS);
        assert!(reg.capacity_error().is_some());

        // The spawn tool refuses past the cap.
        let run = BackgroundRunTool {
            registry: reg.clone(),
        };
        assert!(
            run.execute(json!({ "command": "echo hi" }))
                .await
                .is_error()
        );

        // Cancelling a running task frees a slot.
        let any_id = reg.list()[0].id.clone();
        assert!(reg.cancel(&any_id));
        assert!(reg.capacity_error().is_none());
    }

    #[test]
    fn capability_contributes_background_command() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = BackgroundCapability {
            registry: Arc::new(registry_in(tmp.path())),
        };
        let names: Vec<String> = cap.commands().iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec!["background"]);
    }

    #[tokio::test]
    async fn run_tool_requires_command() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Arc::new(registry_in(tmp.path()));
        let tool = BackgroundRunTool { registry: reg };
        assert!(tool.execute(json!({})).await.is_error());
        assert!(tool.execute(json!({ "command": "  " })).await.is_error());
    }

    #[tokio::test]
    async fn agent_task_records_result_and_child_session() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path()).with_spawner(Arc::new(MockSpawner {
            final_text: Some("found 3 risks in the auth module".into()),
            success: true,
        }));
        let record = reg
            .spawn_agent(
                Some("analyze auth".into()),
                "analyze the auth module".into(),
            )
            .expect("spawner present");
        assert_eq!(record.kind, BackgroundKind::Agent);

        let done = wait_terminal(&reg, &record.id, 100).await;
        assert_eq!(done.status, BackgroundStatus::Completed);
        assert_eq!(done.child_session_id.as_deref(), Some("session_child_test"));
        assert!(done.summary.as_deref().unwrap_or("").contains("3 risks"));

        // The sub-agent's final message and child session id are in the log.
        let (_, output, _) = reg.read_output(&record.id, 64 * 1024).unwrap();
        assert!(output.contains("found 3 risks"), "got: {output}");
        assert!(output.contains("session_child_test"), "got: {output}");
    }

    #[tokio::test]
    async fn agent_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path())
            .with_max_runtime(1)
            .with_spawner(Arc::new(SlowSpawner));
        let record = reg
            .spawn_agent(None, "slow task".into())
            .expect("spawner present");
        let done = wait_terminal(&reg, &record.id, 100).await;
        assert_eq!(done.status, BackgroundStatus::TimedOut);
        let (_, output, _) = reg.read_output(&record.id, 64 * 1024).unwrap();
        assert!(output.contains("timed out"), "got: {output}");
    }

    #[tokio::test]
    async fn agent_failure_marks_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path()).with_spawner(Arc::new(MockSpawner {
            final_text: None,
            success: false,
        }));
        let record = reg
            .spawn_agent(None, "do a thing".into())
            .expect("spawner present");
        let done = wait_terminal(&reg, &record.id, 100).await;
        assert_eq!(done.status, BackgroundStatus::Failed);
    }

    #[tokio::test]
    async fn agent_unavailable_without_spawner() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path());
        // No spawner attached → spawning a sub-agent errors and the tool is hidden.
        assert!(!reg.can_spawn_agents());
        assert!(reg.spawn_agent(None, "x".into()).is_err());

        let cap = BackgroundCapability {
            registry: Arc::new(reg),
        };
        let names: Vec<String> = cap.tools().iter().map(|t| t.name().to_string()).collect();
        assert!(!names.contains(&"background_agent".to_string()));
        assert_eq!(names.len(), 4);
    }

    #[tokio::test]
    async fn agent_tool_offered_when_spawner_present() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = registry_in(tmp.path()).with_spawner(Arc::new(MockSpawner {
            final_text: None,
            success: true,
        }));
        let cap = BackgroundCapability {
            registry: Arc::new(reg),
        };
        let names: Vec<String> = cap.tools().iter().map(|t| t.name().to_string()).collect();
        assert!(names.contains(&"background_agent".to_string()));
        assert_eq!(names.len(), 5);
    }
}

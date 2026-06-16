// The `background` capability — generic background execution for yolop.
//
// A *background task* is a unit of work that runs detached from the foreground
// turn: it has an id, a kind, a lifecycle status, captured output, and is
// cancellable and observable. v1 ships one kind — `script` (a shell command
// that outlives the turn, e.g. `gh pr checks --watch` waiting on CI). Background
// sub-agents are a planned second kind that reuses this same registry, record,
// and surfaces (see specs/background.md).
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
use everruns_core::tools::{Tool, ToolExecutionResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
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

// ---------- data model ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundKind {
    /// A shell command run via `bash -lc` from the workspace root.
    Script,
}

impl BackgroundKind {
    fn label(self) -> &'static str {
        match self {
            BackgroundKind::Script => "script",
        }
    }
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
    #[cfg(test)]
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
    /// Live task handles, keyed by id. Only present while a task is in-process;
    /// a finished handle is harmless to keep but `cancel` removes it. Not
    /// persisted — handles cannot outlive the process.
    handles: HashMap<String, JoinHandle<()>>,
}

/// Per-session owner of background tasks. Cheap to clone via the inner `Arc`.
pub struct BackgroundRegistry {
    inner: Arc<Mutex<Inner>>,
    dir: PathBuf,
    index_path: PathBuf,
    workspace_root: PathBuf,
    max_runtime_secs: u64,
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

        let registry = Self {
            inner: Arc::new(Mutex::new(Inner {
                records,
                handles: HashMap::new(),
            })),
            dir,
            index_path,
            workspace_root,
            max_runtime_secs: DEFAULT_MAX_RUNTIME_SECS,
        };
        if corrected {
            registry.persist();
        }
        registry
    }

    #[cfg(test)]
    fn with_max_runtime(mut self, secs: u64) -> Self {
        self.max_runtime_secs = secs;
        self
    }

    /// Snapshot of all tasks, most-recently-updated first.
    pub fn list(&self) -> Vec<BackgroundRecord> {
        let guard = self.inner.lock().expect("background lock poisoned");
        let mut records = guard.records.clone();
        records.sort_by(|a, b| b.updated.cmp(&a.updated));
        records
    }

    /// Fetch one task by id.
    pub fn get(&self, id: &str) -> Option<BackgroundRecord> {
        let guard = self.inner.lock().expect("background lock poisoned");
        guard.records.iter().find(|r| r.id == id).cloned()
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
                r.status = outcome.status;
                r.exit_code = outcome.exit_code;
                r.summary = Some(outcome.summary.clone());
            });
        });

        let mut guard = self.inner.lock().expect("background lock poisoned");
        guard.handles.insert(id, handle);
        record
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
             list with `background_list`, read full output with `background_output`, cancel with \
             `background_cancel`. A `completed`/`failed` task's result is ready to read NOW.\n",
        );
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

/// Mutate one record under the lock, stamp `updated`, and persist.
fn update_record(
    inner: &Arc<Mutex<Inner>>,
    index_path: &Path,
    id: &str,
    f: impl FnOnce(&mut BackgroundRecord),
) {
    let records = {
        let mut guard = inner.lock().expect("background lock poisoned");
        if let Some(record) = guard.records.iter_mut().find(|r| r.id == id) {
            f(record);
            record.updated = Utc::now();
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------- persistence ----------

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

    let tmp = parent.join(format!(".{INDEX_FILE}.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
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
        vec![
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
        ]
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
    async fn run_tool_requires_command() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Arc::new(registry_in(tmp.path()));
        let tool = BackgroundRunTool { registry: reg };
        assert!(tool.execute(json!({})).await.is_error());
        assert!(tool.execute(json!({ "command": "  " })).await.is_error());
    }
}

// Per-session JSONL event log.
//
// Each session writes its replay-relevant events to
// `<sessions_dir>/<session_id>/events.jsonl`, one serialized `Event` per line.
// `--session <id>` reuses the same SessionId on the next run and replays
// the file: events are read back and messages derived from those events
// are seeded into the message store so the conversation history shows up
// in the next turn's context.
//
// JSONL is chosen because `Event` is already `Serialize + Deserialize`
// and the format degrades gracefully — a half-written line at the end is
// a parse error on one line, not a corrupt file. The writer flushes after
// every line so a crash mid-session loses at most the event in flight.
//
// Concerns explicitly handled below:
// * Owner-only on Unix: `events.jsonl` is created with `0o600` and the
//   file mode is re-tightened to `0o600` on every open; the per-session
//   folder is `chmod`-ed to `0o700` on open as well. The re-tightening
//   matters because `OpenOptionsExt::mode` only applies on create —
//   without it, a legacy file or one loosened out-of-band would keep
//   its prior mode on resume. Session logs contain prompts, tool
//   arguments, and tool output, plus the reasoning artifacts described
//   below.
// * Replay keeps event types that (a) round-trip into the conversation
//   (`input.message`, `output.message.completed`, `tool.completed`) and
//   (b) the agent needs to restore the live transcript view and provider
//   continuation state on resume (`reason.completed`, `reason.item`).
//   Semantic `session.title.updated` events are also retained so the latest
//   title can repair the `workspace.json` projection after interruption.
//   Streaming `*.delta` events have no replay value and would otherwise
//   inflate the log O(n²) for long streamed responses.
// * Assistant `thinking` / `thinking_signature` fields ARE persisted in
//   yolop's per-session JSONL. The per-session folder is the local
//   private session store (owner-only on Unix, see above) and provider
//   continuation on resume requires the signature/encrypted_content
//   (e.g. OpenAI Responses threads the encrypted reasoning context back
//   via `thinking_signature`). The contract is local-store, not
//   user-facing transcript export — see the yolop README for the
//   public/private distinction.
// * Replay rejects events whose `session_id` doesn't match the resumed
//   session — guards against accidentally merging logs across sessions.
// * On open, if the file does not end with `\n` (previous run crashed
//   mid-write), append one before any new line is added — prevents the
//   first new event from being concatenated onto a partial tail.
// * Sequence numbers continue from the replayed maximum rather than
//   restarting from 0, so `Event.sequence` stays monotonic within a
//   session across resumes.
//
// Concurrency:
// * Intra-process: the file handle sits behind a `tokio::Mutex` so emits
//   serialize even when tools fire events from many tasks.
// * Inter-process: an advisory exclusive flock (`File::try_lock`) is acquired
//   in the sessions root's non-discoverable `.locks/` directory before the
//   lazy event log exists. Existing event logs are locked directly as well. A
//   second `yolop --session <same-id>` fails fast instead of interleaving.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use everruns_core::EventEmitter;
use everruns_core::{ContentPart, Message};
use everruns_core::{
    Event, EventData, EventRequest, INPUT_MESSAGE, OUTPUT_MESSAGE_COMPLETED,
    OutputMessageCompletedData, REASON_COMPLETED, REASON_ITEM, SESSION_TITLE_UPDATED,
    TOOL_COMPLETED,
};
use everruns_host::{
    EventCursor, EventDurability, EventLog, EventLogError, EventPage, EventReadRequest,
    EventReader, EventSink, EventSinkError,
};
use everruns_provider::ToolResultImage;
use everruns_provider::typed_id::EventId;
use everruns_provider::typed_id::SessionId;
use everruns_provider::{AgentLoopError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, RwLock, broadcast};

/// Capacity of the live event broadcast. Sized to absorb a few hundred
/// rapid-fire delta events from one LLM turn without the TUI receiver
/// lagging; on overflow the receiver gets `Lagged` and we fall back to
/// catch-up via `runtime.events()`.
const EVENT_BROADCAST_CAPACITY: usize = 1024;

/// Default location for yolop's per-session storage folders: `sessions/` under
/// the data root (`~/.local/share/yolop/` on Linux,
/// `~/Library/Application Support/yolop/` on macOS, `%APPDATA%\yolop\` on
/// Windows), which `--data-dir` / `YOLOP_DATA_DIR` can point elsewhere (see
/// `crate::config::paths`).
///
/// Returns an error when the platform data dir can't be resolved — we
/// intentionally do NOT fall back to the current working directory,
/// because the cwd for yolop is usually the user's workspace and we
/// don't want sensitive session logs landing in the repo.
pub fn default_sessions_dir() -> Result<PathBuf> {
    crate::config::paths::data_dir()
        .map(|p| p.join("sessions"))
        .ok_or_else(|| {
            AgentLoopError::config(
                "could not resolve a platform data directory for session logs; \
                 pass --session-dir <PATH> explicitly",
            )
        })
}

pub fn session_dir_path(sessions_dir: &Path, session_id: SessionId) -> PathBuf {
    sessions_dir.join(session_id.to_string())
}

pub fn session_log_path(session_dir: &Path) -> PathBuf {
    session_dir.join("events.jsonl")
}

fn session_workspace_path(session_dir: &Path) -> PathBuf {
    session_dir.join("workspace.json")
}

pub(crate) struct SessionMaterializer {
    session_dir: PathBuf,
    initial_workspace: StdMutex<Option<SessionWorkspaceMetadata>>,
}

impl SessionMaterializer {
    pub(crate) fn new(
        session_dir: PathBuf,
        initial_workspace: Option<SessionWorkspaceMetadata>,
    ) -> Self {
        Self {
            session_dir,
            initial_workspace: StdMutex::new(initial_workspace),
        }
    }

    pub(crate) fn ensure(&self) -> Result<()> {
        if session_workspace_path(&self.session_dir).exists() {
            return Ok(());
        }
        let mut initial = self
            .initial_workspace
            .lock()
            .expect("session materializer lock poisoned");
        if let Some(metadata) = initial.as_ref() {
            write_session_workspace(&self.session_dir, metadata)?;
            initial.take();
        }
        Ok(())
    }
}

pub fn legacy_session_log_path(sessions_dir: &Path, session_id: SessionId) -> PathBuf {
    sessions_dir.join(format!("{session_id}.jsonl"))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorktreeMetadata {
    pub path: PathBuf,
    pub branch: String,
    pub base_ref: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionWorkspaceMetadata {
    #[serde(alias = "workspace_root")]
    pub active_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_repo_root: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub session_kind: SessionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeMetadata>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Interactive,
    Print,
    Nested,
    Acp,
    Test,
    Automated,
}

impl SessionWorkspaceMetadata {
    pub fn new(active_root: PathBuf, repo_root: Option<PathBuf>) -> Self {
        let now = Utc::now();
        let canonical_repo_root = repo_root.as_ref().map(|path| canonicalize_lossy(path));
        let project_id = canonical_repo_root
            .as_ref()
            .map(|path| format!("file:{}", path.display()));

        Self {
            active_root,
            repo_root,
            canonical_repo_root,
            project_id,
            title: None,
            summary: None,
            created_at: Some(now),
            updated_at: Some(now),
            session_kind: SessionKind::Interactive,
            parent_session_id: None,
            worktree: None,
        }
    }

    pub fn apply_initial_prompt(&mut self, prompt: &str) {
        let title = prompt_title(prompt);
        if title.is_empty() {
            return;
        }
        if self.title.is_none() {
            self.title = Some(title.clone());
        }
        if self.summary.is_none() {
            self.summary = Some(title);
        }
    }
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn prompt_title(prompt: &str) -> String {
    const MAX_TITLE_CHARS: usize = 80;
    let collapsed = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let title: String = chars.by_ref().take(MAX_TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{title}…")
    } else {
        title
    }
}

pub fn read_session_workspace_metadata(
    session_dir: &Path,
) -> Result<Option<SessionWorkspaceMetadata>> {
    let path = session_workspace_path(session_dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "ignoring unreadable session workspace metadata"
            );
            return Ok(None);
        }
    };
    let metadata: SessionWorkspaceMetadata = match serde_json::from_slice(&bytes) {
        Ok(metadata) => metadata,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "ignoring malformed session workspace metadata"
            );
            return Ok(None);
        }
    };
    Ok(Some(metadata))
}

pub fn read_session_workspace(session_dir: &Path) -> Result<Option<PathBuf>> {
    Ok(read_session_workspace_metadata(session_dir)?.map(|m| m.active_root))
}

pub fn write_session_workspace(
    session_dir: &Path,
    metadata: &SessionWorkspaceMetadata,
) -> Result<()> {
    std::fs::create_dir_all(session_dir).map_err(|e| {
        AgentLoopError::config(format!("create session dir {}: {e}", session_dir.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(session_dir, std::fs::Permissions::from_mode(0o700)).map_err(
            |e| {
                AgentLoopError::config(format!(
                    "tighten session dir permissions on {}: {e}",
                    session_dir.display()
                ))
            },
        )?;
    }
    let path = session_workspace_path(session_dir);
    let mut metadata = metadata.clone();
    metadata.updated_at = Some(Utc::now());
    if metadata.created_at.is_none() {
        metadata.created_at = metadata.updated_at;
    }
    let bytes = serde_json::to_vec_pretty(&metadata)
        .map_err(|e| AgentLoopError::config(format!("serialize session workspace: {e}")))?;
    write_private_file(&path, &bytes)
}

pub fn update_session_workspace_title(session_dir: &Path, title: &str) -> Result<bool> {
    let Some(mut metadata) = read_session_workspace_metadata(session_dir)? else {
        return Err(AgentLoopError::config(format!(
            "session workspace metadata not found: {}",
            session_workspace_path(session_dir).display()
        )));
    };
    if metadata.title.as_deref() == Some(title) {
        return Ok(false);
    }
    metadata.title = Some(title.to_string());
    write_session_workspace(session_dir, &metadata)?;
    Ok(true)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path).map_err(|e| {
        AgentLoopError::config(format!("open private file {}: {e}", path.display()))
    })?;
    file.write_all(bytes).map_err(|e| {
        AgentLoopError::config(format!("write private file {}: {e}", path.display()))
    })?;
    file.write_all(b"\n").map_err(|e| {
        AgentLoopError::config(format!("write private file {}: {e}", path.display()))
    })?;
    file.flush().map_err(|e| {
        AgentLoopError::config(format!("flush private file {}: {e}", path.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            AgentLoopError::config(format!(
                "tighten private file permissions on {}: {e}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

/// Copy a pre-folder-layout session log into the current session folder.
///
/// The old layout wrote `<sessions_dir>/<session_id>.jsonl`; the current
/// layout writes `<sessions_dir>/<session_id>/events.jsonl`. Copying instead
/// of renaming keeps older yolop binaries able to read the legacy file.
pub fn migrate_legacy_session_log(
    sessions_dir: &Path,
    session_dir: &Path,
    session_id: SessionId,
) -> Result<Option<PathBuf>> {
    let current = session_log_path(session_dir);
    if current.exists() {
        return Ok(None);
    }
    let legacy = legacy_session_log_path(sessions_dir, session_id);
    if !legacy.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(session_dir).map_err(|e| {
        AgentLoopError::config(format!("create session dir {}: {e}", session_dir.display()))
    })?;
    std::fs::copy(&legacy, &current).map_err(|e| {
        AgentLoopError::config(format!(
            "migrate legacy session log {} to {}: {e}",
            legacy.display(),
            current.display()
        ))
    })?;
    Ok(Some(legacy))
}

/// Result of replaying a JSONL session log from disk.
#[derive(Debug, Default)]
pub struct ReplayedSession {
    pub events: Vec<Event>,
    pub messages: Vec<Message>,
    /// Highest `Event.sequence` value found in the file (None if no
    /// events had sequence numbers). The new emitter resumes from
    /// `max_sequence + 1`.
    pub max_sequence: Option<i32>,
}

/// Read a session log into memory. Missing files return an empty replay.
/// Malformed lines and events for a different session are skipped with
/// a tracing warning; a half-written tail line shouldn't take down the
/// next session.
pub fn replay(path: &Path, expected: SessionId) -> Result<ReplayedSession> {
    if !path.exists() {
        return Ok(ReplayedSession::default());
    }
    let file = File::open(path)
        .map_err(|e| AgentLoopError::config(format!("open session log {}: {e}", path.display())))?;
    let mut out = ReplayedSession::default();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let Ok(line) = line else {
            tracing::warn!(
                line = i + 1,
                "session log read error; stopping replay early"
            );
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(&line) {
            Ok(event) => {
                if event.session_id != expected {
                    tracing::warn!(
                        line = i + 1,
                        found = %event.session_id,
                        expected = %expected,
                        "session log line belongs to a different session; skipping"
                    );
                    continue;
                }
                if let Some(seq) = event.sequence {
                    out.max_sequence = Some(out.max_sequence.map_or(seq, |m| m.max(seq)));
                }
                if let Some(message) = message_from_event(&event.data) {
                    out.messages.push(message);
                }
                out.events.push(event);
            }
            Err(e) => {
                tracing::warn!(line = i + 1, error = %e, "skipping malformed session-log line");
            }
        }
    }
    Ok(out)
}

/// Event types that are useful to keep on disk: those that replay can map
/// back into the conversation (`input.message`, `output.message.completed`,
/// `tool.completed`) plus the agent reasoning artifacts the CLI uses to
/// restore the live transcript view and provider continuation state on
/// resume (`reason.completed` carries the safe `text_preview` narration,
/// `reason.item` carries opaque/encrypted reasoning context curated by the
/// provider), plus semantic title updates used to repair workspace metadata.
/// Streaming `*.delta` events and pure lifecycle markers
/// (`reason.started`, `reason.thinking.*`, `output.message.started`) are
/// dropped — they are live status signals only and the delta types would
/// bloat the log O(n²) since each delta carries the accumulated text so
/// far.
fn is_replay_relevant(event_type: &str) -> bool {
    matches!(
        event_type,
        INPUT_MESSAGE
            | OUTPUT_MESSAGE_COMPLETED
            | TOOL_COMPLETED
            | REASON_COMPLETED
            | REASON_ITEM
            | SESSION_TITLE_UPDATED
    )
}

pub(crate) fn latest_session_title(events: &[Event]) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.data {
        EventData::SessionTitleUpdated(data) => Some(data.title.clone()),
        _ => None,
    })
}

/// EventEmitter that appends replay-relevant events as JSONL to a file
/// and mirrors all events into an in-memory vec for `events()` queries.
///
/// Owns its own sequence counter so resumed sessions continue past the
/// max sequence found in the replayed log (rather than restarting at 1
/// and producing non-monotonic per-session sequences).
pub struct JsonlEventEmitter {
    events: Arc<RwLock<Vec<Event>>>,
    sequence: Arc<AtomicI32>,
    persisted_sequence: Arc<AtomicI32>,
    file: Arc<Mutex<Option<File>>>,
    path: PathBuf,
    _session_lock: File,
    materializer: Arc<SessionMaterializer>,
    session_dir: PathBuf,
    /// Live fan-out of every emitted event (including deltas) for in-process
    /// subscribers like the TUI's streaming renderer. Filesystem persistence
    /// still filters by `is_replay_relevant`; this channel does not.
    live: broadcast::Sender<Event>,
}

impl JsonlEventEmitter {
    /// Open (or create) the session-log file and prepare the fan-out.
    /// `start_sequence` is the value `Event.sequence` should take on
    /// the next emitted event (1 for a fresh session, max_replayed + 1
    /// for a resume).
    #[cfg(test)]
    pub fn open(path: &Path, start_sequence: i32) -> Result<Self> {
        let session_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::open_with_materializer(
            path,
            start_sequence,
            Arc::new(SessionMaterializer::new(session_dir, None)),
        )
    }

    pub(crate) fn open_with_materializer(
        path: &Path,
        start_sequence: i32,
        materializer: Arc<SessionMaterializer>,
    ) -> Result<Self> {
        let session_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let sessions_dir = session_dir.parent().unwrap_or_else(|| Path::new("."));
        let lock_dir = sessions_dir.join(".locks");
        std::fs::create_dir_all(&lock_dir).map_err(|e| {
            AgentLoopError::config(format!(
                "create session lock dir {}: {e}",
                lock_dir.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    AgentLoopError::config(format!(
                        "tighten session lock dir permissions on {}: {e}",
                        lock_dir.display()
                    ))
                },
            )?;
        }
        let lock_name = session_dir
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("session"));
        let lock_path = lock_dir.join(lock_name);
        let session_lock = open_private_append(&lock_path)?;
        try_lock_session_file(&session_lock, &lock_path)?;

        let file = if path.exists() {
            Some(open_session_log(path)?)
        } else {
            None
        };

        let (live, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        Ok(Self {
            events: Arc::new(RwLock::new(Vec::new())),
            sequence: Arc::new(AtomicI32::new(start_sequence.saturating_sub(1))),
            persisted_sequence: Arc::new(AtomicI32::new(start_sequence.saturating_sub(1))),
            file: Arc::new(Mutex::new(file)),
            path: path.to_path_buf(),
            _session_lock: session_lock,
            materializer,
            session_dir: session_dir.to_path_buf(),
            live,
        })
    }

    async fn ensure_log_file(&self) -> Result<tokio::sync::MutexGuard<'_, Option<File>>> {
        let mut file = self.file.lock().await;
        if file.is_none() {
            self.materializer.ensure()?;
            *file = Some(open_session_log(&self.path)?);
        }
        Ok(file)
    }

    /// Subscribe to live events as they are emitted. Returns a receiver
    /// that begins delivering events from this point forward — replayed
    /// history seeded via [`Self::seed_replayed`] is NOT re-broadcast.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.live.subscribe()
    }

    /// Push events read back from disk into the in-memory vec that
    /// `EventBus::collected_events()` returns. Does NOT re-write them to
    /// the JSONL file (they're already there) — purely a seeding step so
    /// `runtime.events()` after resume returns the full session history
    /// instead of starting empty.
    pub async fn seed_replayed(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        // The canonical event log requires a sequence on every event, and a
        // reader must not silently drop the ones without it — that would turn a
        // log written before sequencing into an empty resume.
        //
        // Backfill in order rather than allocating from the live counter: the
        // emitter was opened with the sequence *after* the replayed events, so
        // those events belong below that floor. Consuming counter values here
        // would push new appends past it and leave a gap.
        let mut normalized = Vec::with_capacity(events.len());
        let mut last = 0i32;
        for mut event in events {
            match event.sequence {
                Some(sequence) => last = sequence,
                None => {
                    last = last.saturating_add(1);
                    event.sequence = Some(last);
                }
            }
            normalized.push(event);
        }
        if let Some(highest) = normalized.iter().filter_map(|event| event.sequence).max() {
            self.sequence.fetch_max(highest, Ordering::AcqRel);
            self.persisted_sequence.fetch_max(highest, Ordering::AcqRel);
        }
        self.events.write().await.extend(normalized);
    }

    /// Replace the model-visible branch after a conversation rewind. The
    /// discarded suffix remains durable in `events.jsonl`; only the live view
    /// queried by the runtime and transcript changes.
    pub async fn replace_collected_events(&self, events: Vec<Event>) {
        *self.events.write().await = events;
    }

    /// Highest sequence allocated in this session, including abandoned
    /// branches. New branches continue from this value so event ordering stays
    /// globally monotonic.
    pub fn last_sequence(&self) -> i32 {
        self.persisted_sequence.load(Ordering::Acquire)
    }

    pub(crate) fn materializer(&self) -> Arc<SessionMaterializer> {
        self.materializer.clone()
    }
}

fn open_private_append(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    // Windows' file-locking API rejects an append-only handle. Match the
    // event-log handle's read + append access so `File::try_lock` works on
    // every supported platform.
    opts.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(path).map_err(|e| {
        AgentLoopError::config(format!("open private file {}: {e}", path.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            AgentLoopError::config(format!(
                "tighten private file permissions on {}: {e}",
                path.display()
            ))
        })?;
    }
    Ok(file)
}

fn try_lock_session_file(file: &File, path: &Path) -> Result<()> {
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(AgentLoopError::config(format!(
                "another yolop process is already writing {}; \
                     refusing to share a session log",
                path.display()
            )));
        }
        Err(TryLockError::Error(e)) => {
            return Err(AgentLoopError::config(format!(
                "lock session log {}: {e}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn open_session_log(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AgentLoopError::config(format!("create session log dir {}: {e}", parent.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    AgentLoopError::config(format!(
                        "tighten session dir permissions on {}: {e}",
                        parent.display()
                    ))
                },
            )?;
        }
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| AgentLoopError::config(format!("open session log {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            AgentLoopError::config(format!(
                "tighten session log permissions on {}: {e}",
                path.display()
            ))
        })?;
    }
    try_lock_session_file(&file, path)?;

    // Repair half-written tail: if the file is non-empty and does
    // NOT end with '\n', the previous process crashed after writing
    // a partial JSON object. A naive append would concatenate the
    // new line onto that partial tail and produce a corrupt entry.
    // Add a leading '\n' so the partial tail becomes its own
    // (malformed, skipped) line and our new line stays clean.
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|e| AgentLoopError::config(format!("stat session log: {e}")))?;
    if len > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(len - 1))
            .and_then(|_| std::io::Read::read_exact(&mut file, &mut last))
            .map_err(|e| AgentLoopError::config(format!("read session log tail: {e}")))?;
        if last[0] != b'\n' {
            tracing::warn!(
                "session log {} ends without newline; repairing tail before append",
                path.display()
            );
            writeln!(file)
                .map_err(|e| AgentLoopError::config(format!("repair session log tail: {e}")))?;
        }
    }

    Ok(file)
}

impl JsonlEventEmitter {
    /// Commit one event: assign id and sequence, record it in the active-branch
    /// view, and persist it when it is replay-relevant.
    ///
    /// This is the whole write path. It deliberately does not fan out to live
    /// subscribers: under the canonical event model the host appends here and
    /// then hands the finalized envelope to an [`EventSink`], which is where
    /// yolop's broadcast lives. `emit` below keeps both halves together for the
    /// few callers that drive the emitter directly.
    async fn commit(&self, request: EventRequest) -> Result<Event> {
        // Assign id + sequence ourselves so we own the monotonic
        // sequence even across resumes.
        let previous = self
            .sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |sequence| {
                Some(sequence.saturating_add(1))
            })
            .expect("sequence update closure always succeeds");
        let seq_val = previous.saturating_add(1);

        let mut event = request.into_event(EventId::new(), seq_val);
        // `into_event` may have set its own timestamp; ensure we always
        // record one for replay ordering.
        if event.ts.timestamp() == 0 {
            event.ts = Utc::now();
        }
        self.events.write().await.push(event.clone());

        if is_replay_relevant(&event.event_type) {
            // yolop's per-session JSONL is the local session store; we
            // persist `thinking` / `thinking_signature` and opaque
            // `reason.item` content as-is so provider continuation
            // (e.g. OpenAI Responses replays encrypted reasoning via
            // `thinking_signature`) works after `--session <id>` resume.
            // Privacy lives in the 0o600 file mode and per-user
            // platform data dir, not in field stripping.
            let line = serde_json::to_string(&event).map_err(|e| {
                AgentLoopError::config(format!("serialize event for session log: {e}"))
            })?;
            let mut file = self.ensure_log_file().await?;
            let file = file.as_mut().expect("session log initialized");
            writeln!(file, "{line}")
                .map_err(|e| AgentLoopError::config(format!("write session log line: {e}")))?;
            file.flush()
                .map_err(|e| AgentLoopError::config(format!("flush session log: {e}")))?;
            self.persisted_sequence.fetch_max(seq_val, Ordering::AcqRel);
        }

        if let EventData::SessionTitleUpdated(data) = &event.data
            && let Err(error) = update_session_workspace_title(&self.session_dir, &data.title)
        {
            tracing::warn!(
                session_id = %event.session_id,
                error = %error,
                "failed to project session title into workspace metadata"
            );
        }

        Ok(event)
    }

    /// Events on the branch currently visible to the model, in sequence order.
    ///
    /// Was `EventBus::collected_events`; the trait is gone with the canonical
    /// event model, but yolop's own callers (checkpointing, transcript) still
    /// want the whole active branch in one shot rather than paged.
    #[cfg(test)]
    pub async fn collected_events(&self) -> Vec<Event> {
        self.events.read().await.clone()
    }
}

#[async_trait]
impl EventEmitter for JsonlEventEmitter {
    async fn emit(&self, request: EventRequest) -> Result<Event> {
        let event = self.commit(request).await?;
        // Fan out to live subscribers. `send` errors only when there are
        // no receivers, which is the common steady state — ignore.
        let _ = self.live.send(event.clone());
        Ok(event)
    }
}

#[async_trait]
impl EventReader for JsonlEventEmitter {
    /// Page over the active branch.
    ///
    /// This is where yolop's rewind survives the move to an append-only
    /// canonical log. `self.events` holds only the branch the model can see —
    /// `replace_collected_events` swaps it after a rewind, while the discarded
    /// suffix stays durable in `events.jsonl` for redo. Paging over that view
    /// therefore hands the runtime the active branch without the log needing
    /// any truncation or mutation contract.
    async fn read_page(
        &self,
        request: EventReadRequest,
    ) -> std::result::Result<EventPage, EventLogError> {
        let session_id = request.session_id();
        if let Some(cursor) = request.cursor()
            && cursor.session_id() != session_id
        {
            return Err(EventLogError::CrossSessionCursor {
                detail: format!(
                    "cursor for session {} presented to session {session_id}",
                    cursor.session_id()
                ),
            });
        }

        let events = self.events.read().await;
        let visible: Vec<&Event> = events
            .iter()
            .filter(|event| event.session_id == session_id)
            .collect();

        // A continuation stays pinned to the watermark its first page fixed, so
        // appends made since then stay invisible. A fresh read pins to the
        // highest sequence present right now; `0` means nothing durable yet.
        let watermark = match request
            .cursor()
            .and_then(EventCursor::snapshot_high_watermark)
        {
            Some(pinned) => pinned,
            None => visible
                .iter()
                .filter_map(|event| event.sequence)
                .max()
                .unwrap_or(0),
        };
        let after = request
            .cursor()
            .map(EventCursor::after_sequence)
            .unwrap_or(0);

        let mut selected: Vec<Event> = visible
            .into_iter()
            .filter(|event| {
                event
                    .sequence
                    .is_some_and(|sequence| sequence > after && sequence <= watermark)
            })
            .cloned()
            .collect();
        selected.sort_by_key(|event| event.sequence);

        let limit = request.limit().get();
        let next_cursor = if selected.len() > limit {
            selected.truncate(limit);
            let last = selected
                .last()
                .and_then(|event| event.sequence)
                .unwrap_or(after);
            Some(EventCursor::continuation(session_id, last, watermark)?)
        } else {
            None
        };

        EventPage::new(selected, next_cursor, watermark)
    }
}

#[async_trait]
impl EventLog for JsonlEventEmitter {
    async fn append(&self, request: EventRequest) -> std::result::Result<Event, EventLogError> {
        self.commit(request)
            .await
            .map_err(|error| EventLogError::Backend {
                detail: error.to_string(),
            })
    }

    /// Replay-relevant events are written and flushed before the append is
    /// acknowledged, so an accepted write survives a crash.
    fn durability(&self) -> EventDurability {
        EventDurability::CrashDurable
    }
}

impl EventSink for JsonlEventEmitter {
    /// Live observation for the TUI and ACP streams. Lossy by contract, and
    /// yolop's broadcast matches: `send` fails only when nothing is listening,
    /// which is the steady state for a headless run.
    fn try_send(&self, event: Event) -> std::result::Result<(), EventSinkError> {
        let _ = self.live.send(event);
        Ok(())
    }
}

// ---------- event → message mapping ----------
//
// `crates/runtime/src/runtime.rs` has the same logic in a private fn;
// copied here for the replay path. The three event types matched are
// exactly those `is_replay_relevant` lets through to disk.

fn message_from_event(data: &EventData) -> Option<Message> {
    match data {
        EventData::InputMessage(d) => Some(d.message.clone()),
        EventData::OutputMessageCompleted(OutputMessageCompletedData { message, .. }) => {
            Some(message.clone())
        }
        EventData::ToolCompleted(d) => Some(tool_completed_to_message(d.clone())),
        _ => None,
    }
}

// Only checkpoint tests project messages from events now: the runtime reads the
// conversation back through the event log itself.
#[cfg(test)]
pub(crate) fn messages_from_events(events: &[Event]) -> Vec<Message> {
    events
        .iter()
        .filter_map(|event| message_from_event(&event.data))
        .collect()
}

fn tool_completed_to_message(data: everruns_core::events::ToolCompletedData) -> Message {
    let mut images: Vec<ToolResultImage> = Vec::new();
    let result = data.result.map(|parts| {
        for part in &parts {
            if let ContentPart::Image(img) = part
                && let (Some(base64), Some(media_type)) = (&img.base64, &img.media_type)
            {
                images.push(ToolResultImage {
                    base64: base64.clone(),
                    media_type: media_type.clone(),
                });
            }
        }

        let text_parts: Vec<&ContentPart> = parts
            .iter()
            .filter(|part| matches!(part, ContentPart::Text(_)))
            .collect();
        if text_parts.len() == 1
            && let ContentPart::Text(text) = text_parts[0]
        {
            return parse_structured_tool_result_text(&text.text);
        }
        if !text_parts.is_empty() {
            serde_json::to_value(&text_parts).unwrap_or_default()
        } else {
            serde_json::Value::Null
        }
    });

    if images.is_empty() {
        Message::tool_result(&data.tool_call_id, result, data.error)
    } else {
        Message::tool_result_with_images(&data.tool_call_id, result, images)
    }
}

fn parse_structured_tool_result_text(text: &str) -> serde_json::Value {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
        return serde_json::Value::String(text.to_string());
    }

    match serde_json::from_str(text) {
        Ok(value @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => value,
        _ => serde_json::Value::String(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::Message;
    use everruns_core::{
        EventContext, InputMessageData, OutputMessageCompletedData, SessionTitleUpdatedData,
        ToolCompletedData,
    };
    use everruns_host::EventReadLimit;

    fn input_event(session_id: SessionId, text: &str) -> Event {
        Event::new(
            session_id,
            EventContext::default(),
            InputMessageData::new(Message::user(text)),
        )
    }

    async fn read_all(emitter: &JsonlEventEmitter, session_id: SessionId) -> Vec<Event> {
        let request = EventReadRequest::new(session_id, EventReadLimit::default());
        emitter.read_page(request).await.expect("read page").events
    }

    async fn append_input(emitter: &JsonlEventEmitter, session_id: SessionId, text: &str) -> Event {
        emitter
            .append(EventRequest::new(
                session_id,
                EventContext::default(),
                InputMessageData::new(Message::user(text)),
            ))
            .await
            .expect("append")
    }

    /// The property that lets rewind survive an append-only canonical log: the
    /// log projects the active branch, so a discarded suffix stops being
    /// model-visible without the log needing truncation.
    #[tokio::test]
    async fn read_page_projects_only_the_active_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(9911);
        let path = session_log_path(&session_dir_path(dir.path(), session_id));
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        append_input(&emitter, session_id, "kept").await;
        append_input(&emitter, session_id, "discarded").await;
        assert_eq!(read_all(&emitter, session_id).await.len(), 2);

        let kept: Vec<Event> = emitter
            .collected_events()
            .await
            .into_iter()
            .take(1)
            .collect();
        emitter.replace_collected_events(kept).await;

        let visible = read_all(&emitter, session_id).await;
        assert_eq!(visible.len(), 1, "rewind must hide the discarded suffix");
        assert_eq!(visible[0].sequence, Some(1));
    }

    /// A continuation is pinned to the watermark its first page fixed, so an
    /// append made mid-read is not observable through that cursor.
    #[tokio::test]
    async fn continuation_cannot_observe_a_concurrent_append() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(9912);
        let path = session_log_path(&session_dir_path(dir.path(), session_id));
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");
        for index in 0..3 {
            append_input(&emitter, session_id, &format!("m{index}")).await;
        }

        let limit = EventReadLimit::new(2).expect("limit");
        let first = emitter
            .read_page(EventReadRequest::new(session_id, limit))
            .await
            .expect("first page");
        assert_eq!(first.events.len(), 2);
        let cursor = first.next_cursor.expect("more remains");

        append_input(&emitter, session_id, "arrived-mid-read").await;

        let rest = emitter
            .read_page(EventReadRequest::from_cursor(cursor, limit))
            .await
            .expect("continuation");
        assert_eq!(
            rest.events.len(),
            1,
            "snapshot must exclude the concurrent append"
        );
        assert_eq!(rest.events[0].sequence, Some(3));
    }

    /// A cursor minted for another session must be refused rather than
    /// silently returning this session's events.
    #[tokio::test]
    async fn a_cursor_from_another_session_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(9913);
        let other = SessionId::from_seed(9914);
        let path = session_log_path(&session_dir_path(dir.path(), session_id));
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");
        append_input(&emitter, session_id, "one").await;

        let foreign = EventCursor::continuation(other, 0, 1).expect("cursor");
        let error = emitter
            .read_page(
                EventReadRequest::new(session_id, EventReadLimit::default()).with_cursor(foreign),
            )
            .await
            .expect_err("cross-session cursor must be refused");
        assert!(matches!(error, EventLogError::CrossSessionCursor { .. }));
    }

    /// Replayed events that predate sequencing must still be readable, or a
    /// resume from such a log would render an empty transcript.
    #[tokio::test]
    async fn seeded_events_without_a_sequence_are_still_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(9915);
        let path = session_log_path(&session_dir_path(dir.path(), session_id));
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        emitter
            .seed_replayed(vec![
                input_event(session_id, "older"),
                input_event(session_id, "newer"),
            ])
            .await;

        let visible = read_all(&emitter, session_id).await;
        assert_eq!(visible.len(), 2, "sequence-less history must stay visible");
        assert_eq!(visible[0].sequence, Some(1));
        assert_eq!(visible[1].sequence, Some(2));
    }

    fn title_event(session_id: SessionId, previous_title: Option<&str>, title: &str) -> Event {
        Event::new(
            session_id,
            EventContext::default(),
            SessionTitleUpdatedData {
                previous_title: previous_title.map(str::to_string),
                title: title.to_string(),
            },
        )
    }

    #[tokio::test]
    async fn title_event_is_persisted_and_projected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(483);
        let session_dir = session_dir_path(dir.path(), session_id);
        write_session_workspace(
            &session_dir,
            &SessionWorkspaceMetadata::new(dir.path().to_path_buf(), None),
        )
        .expect("seed workspace metadata");
        let emitter = JsonlEventEmitter::open(&session_log_path(&session_dir), 1).expect("open");

        emitter
            .emit(EventRequest::new(
                session_id,
                EventContext::default(),
                SessionTitleUpdatedData {
                    previous_title: None,
                    title: "Automatic session titles".to_string(),
                },
            ))
            .await
            .expect("emit title update");

        let replayed = replay(&session_log_path(&session_dir), session_id).expect("replay");
        assert_eq!(
            latest_session_title(&replayed.events).as_deref(),
            Some("Automatic session titles")
        );
        let metadata = read_session_workspace_metadata(&session_dir)
            .expect("read metadata")
            .expect("metadata present");
        assert_eq!(metadata.title.as_deref(), Some("Automatic session titles"));
    }

    #[test]
    fn latest_title_uses_last_semantic_update() {
        let session_id = SessionId::from_seed(484);
        let events = vec![
            title_event(session_id, None, "First theme"),
            input_event(session_id, "switch topics"),
            title_event(session_id, Some("First theme"), "Second theme"),
        ];

        assert_eq!(
            latest_session_title(&events).as_deref(),
            Some("Second theme")
        );
    }

    #[tokio::test]
    async fn seed_replayed_populates_collected_events_without_rewrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(482);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);

        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");
        let prior = vec![
            input_event(session_id, "first prior turn"),
            input_event(session_id, "second prior turn"),
        ];
        emitter.seed_replayed(prior.clone()).await;

        // Acceptance: collected_events() returns the seeded events.
        let collected = emitter.collected_events().await;
        assert_eq!(collected.len(), prior.len());
        assert_eq!(collected[0].id, prior[0].id);
        assert_eq!(collected[1].id, prior[1].id);

        // Acceptance: seeding must NOT re-write to the JSONL file.
        // The file was opened fresh and nothing was emitted; lazy persistence
        // should not materialize it at all.
        assert!(
            !path.exists(),
            "seed_replayed must not create or re-persist the event log"
        );
    }

    #[tokio::test]
    async fn seed_replayed_then_emit_keeps_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(4820);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);

        let emitter = JsonlEventEmitter::open(&path, 3).expect("open");
        let prior = vec![input_event(session_id, "a"), input_event(session_id, "b")];
        emitter.seed_replayed(prior.clone()).await;

        let req = EventRequest::new(
            session_id,
            EventContext::default(),
            InputMessageData::new(Message::user("new")),
        );
        let _new = emitter.emit(req).await.expect("emit");

        let collected = emitter.collected_events().await;
        assert_eq!(collected.len(), 3, "seeded + 1 new");
        // New event sequence continues from start_sequence we opened with.
        assert_eq!(collected[2].sequence, Some(3));
    }

    #[test]
    fn migrate_legacy_session_log_copies_flat_jsonl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48200);
        let legacy_path = legacy_session_log_path(dir.path(), session_id);
        std::fs::write(&legacy_path, "legacy event\n").expect("write legacy");

        let session_dir = session_dir_path(dir.path(), session_id);
        let migrated =
            migrate_legacy_session_log(dir.path(), &session_dir, session_id).expect("migrate");
        let current_path = session_log_path(&session_dir);

        assert_eq!(migrated.as_deref(), Some(legacy_path.as_path()));
        assert_eq!(
            std::fs::read_to_string(&current_path).expect("read migrated"),
            "legacy event\n"
        );
        assert!(
            legacy_path.exists(),
            "migration copies instead of renaming for old yolop compatibility"
        );
    }

    #[test]
    fn migrate_legacy_session_log_does_not_overwrite_current_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48201);
        let legacy_path = legacy_session_log_path(dir.path(), session_id);
        std::fs::write(&legacy_path, "legacy event\n").expect("write legacy");
        let session_dir = session_dir_path(dir.path(), session_id);
        std::fs::create_dir_all(&session_dir).expect("create session dir");
        let current_path = session_log_path(&session_dir);
        std::fs::write(&current_path, "current event\n").expect("write current");

        let migrated =
            migrate_legacy_session_log(dir.path(), &session_dir, session_id).expect("migrate");

        assert!(migrated.is_none());
        assert_eq!(
            std::fs::read_to_string(&current_path).expect("read current"),
            "current event\n"
        );
    }

    #[test]
    fn read_session_workspace_ignores_malformed_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48202);
        let session_dir = session_dir_path(dir.path(), session_id);
        std::fs::create_dir_all(&session_dir).expect("create session dir");
        std::fs::write(session_workspace_path(&session_dir), "{not json")
            .expect("write malformed metadata");

        let workspace = read_session_workspace(&session_dir).expect("read workspace metadata");

        assert!(workspace.is_none());
    }

    #[test]
    fn read_legacy_workspace_metadata_defaults_new_fields() {
        let json = r#"{
            "active_root": "/tmp/yolop-workspace",
            "repo_root": "/tmp/yolop-repo"
        }"#;

        let metadata: SessionWorkspaceMetadata =
            serde_json::from_str(json).expect("legacy workspace metadata deserializes");

        assert_eq!(metadata.active_root, PathBuf::from("/tmp/yolop-workspace"));
        assert_eq!(metadata.repo_root, Some(PathBuf::from("/tmp/yolop-repo")));
        assert_eq!(metadata.canonical_repo_root, None);
        assert_eq!(metadata.project_id, None);
        assert_eq!(metadata.title, None);
        assert_eq!(metadata.summary, None);
        assert_eq!(metadata.created_at, None);
        assert_eq!(metadata.updated_at, None);
        assert_eq!(metadata.session_kind, SessionKind::Interactive);
        assert_eq!(metadata.parent_session_id, None);
        assert!(metadata.worktree.is_none());
    }

    #[test]
    fn write_workspace_metadata_persists_agf_listing_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48203);
        let session_dir = session_dir_path(dir.path(), session_id);
        let canonical_dir = dir.path().canonicalize().expect("canonical tempdir");
        let mut metadata = SessionWorkspaceMetadata::new(
            dir.path().join("worktree"),
            Some(dir.path().to_path_buf()),
        );
        metadata.title = Some("Implement workspace metadata".to_string());
        metadata.summary = Some("Add list-friendly Yolop session metadata.".to_string());
        metadata.session_kind = SessionKind::Nested;
        metadata.parent_session_id = Some(SessionId::from_seed(48204));
        metadata.worktree = Some(WorktreeMetadata {
            path: dir.path().join("worktree"),
            branch: "feature/agf-metadata".to_string(),
            base_ref: "origin/main".to_string(),
            slug: "agf-metadata".to_string(),
        });

        write_session_workspace(&session_dir, &metadata).expect("write workspace metadata");
        let written = read_session_workspace_metadata(&session_dir)
            .expect("read workspace metadata")
            .expect("metadata present");

        assert_eq!(
            written.title.as_deref(),
            Some("Implement workspace metadata")
        );
        assert_eq!(
            written.summary.as_deref(),
            Some("Add list-friendly Yolop session metadata.")
        );
        assert_eq!(written.session_kind, SessionKind::Nested);
        assert_eq!(written.parent_session_id, Some(SessionId::from_seed(48204)));
        assert_eq!(written.canonical_repo_root, Some(canonical_dir.clone()));
        assert_eq!(
            written.project_id.as_deref(),
            Some(format!("file:{}", canonical_dir.display()).as_str())
        );
        assert!(written.created_at.is_some(), "created_at is populated");
        assert!(written.updated_at.is_some(), "updated_at is populated");
        assert!(
            written.updated_at >= written.created_at,
            "updated_at should not precede created_at"
        );
        assert_eq!(
            written.worktree.map(|worktree| worktree.branch),
            Some("feature/agf-metadata".to_string())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn first_event_materializes_owner_only_session_storage_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48222);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);

        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");
        assert!(!session_dir.exists(), "open must stay filesystem-lazy");
        emitter
            .emit(EventRequest::new(
                session_id,
                EventContext::default(),
                InputMessageData::new(Message::user("persist me")),
            ))
            .await
            .expect("emit first event");

        let mode = std::fs::metadata(&session_dir)
            .expect("session dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "per-session folder must be owner-only on Unix, got {mode:o}"
        );

        let file_mode = std::fs::metadata(&path)
            .expect("events.jsonl exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "events.jsonl must be owner-only on Unix, got {file_mode:o}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_corrects_loose_events_jsonl_permissions_on_resume() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48224);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);

        std::fs::create_dir_all(&session_dir).expect("pre-create");
        std::fs::write(&path, "").expect("pre-create file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("loosen for test");

        let _emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        let mode = std::fs::metadata(&path)
            .expect("events.jsonl exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "resume must re-tighten an existing loose events.jsonl, got {mode:o}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_corrects_loose_session_dir_permissions_on_resume() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48223);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);

        std::fs::create_dir_all(&session_dir).expect("pre-create");
        std::fs::write(&path, "").expect("pre-create log");
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o755))
            .expect("loosen for test");

        let _emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        let mode = std::fs::metadata(&session_dir)
            .expect("session dir exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o700,
            "resume must re-tighten an existing loose session folder, got {mode:o}"
        );
    }

    #[test]
    fn simultaneous_fresh_opens_fail_without_creating_a_session_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48225);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);

        let first = JsonlEventEmitter::open(&path, 1).expect("first open");
        let error = JsonlEventEmitter::open(&path, 1)
            .err()
            .expect("second open must fail");
        assert!(error.to_string().contains("already writing"), "{error}");
        assert!(
            !session_dir.exists(),
            "coordination locks must not create a discoverable session shell"
        );

        drop(first);
        JsonlEventEmitter::open(&path, 1).expect("lock released after close");
    }

    #[tokio::test]
    async fn first_event_after_crash_tail_is_replayable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(48226);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);
        std::fs::create_dir_all(&session_dir).expect("session dir");
        std::fs::write(&path, "{\"partial\"").expect("partial crash tail");

        let emitter = JsonlEventEmitter::open(&path, 8).expect("resume open");
        emitter
            .emit(EventRequest::new(
                session_id,
                EventContext::default(),
                InputMessageData::new(Message::user("after crash")),
            ))
            .await
            .expect("emit after tail repair");
        drop(emitter);

        let replayed = replay(&path, session_id).expect("replay repaired log");
        assert_eq!(replayed.events.len(), 1);
        assert_eq!(replayed.events[0].sequence, Some(8));
        assert_eq!(replayed.messages[0].text(), Some("after crash"));
    }

    #[tokio::test]
    async fn output_message_thinking_is_persisted_for_provider_continuation() {
        // yolop's per-session JSONL is the local session store: thinking
        // and thinking_signature must round-trip so providers that thread
        // encrypted reasoning context back (e.g. OpenAI Responses via
        // `thinking_signature`) can continue across `--session <id>` resume.
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(4821);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        let mut message = Message::assistant("I will inspect the files.");
        message.thinking = Some("private model reasoning".to_string());
        message.thinking_signature = Some("encrypted-thinking-token".to_string());
        let req = EventRequest::new(
            session_id,
            EventContext::default(),
            OutputMessageCompletedData::new(message),
        );

        emitter.emit(req).await.expect("emit");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("private model reasoning"),
            "session log must persist assistant thinking for restore: {on_disk}"
        );
        assert!(
            on_disk.contains("encrypted-thinking-token"),
            "session log must persist thinking_signature for provider continuation: {on_disk}"
        );

        let replayed = replay(&path, session_id).expect("replay");
        let replayed_message = replayed.messages.first().expect("message replayed");
        assert_eq!(
            replayed_message.thinking.as_deref(),
            Some("private model reasoning")
        );
        assert_eq!(
            replayed_message.thinking_signature.as_deref(),
            Some("encrypted-thinking-token")
        );
    }

    #[tokio::test]
    async fn reason_completed_event_is_persisted_and_replayed() {
        use everruns_core::ReasonCompletedData;

        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(4822);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        let req = EventRequest::new(
            session_id,
            EventContext::default(),
            ReasonCompletedData::success("Will read the lib.rs file", true, 1, Some(120), None),
        );
        emitter.emit(req).await.expect("emit");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("\"reason.completed\""),
            "reason.completed should be persisted to JSONL: {on_disk}"
        );
        assert!(
            on_disk.contains("Will read the lib.rs file"),
            "text_preview narration should round-trip on disk: {on_disk}"
        );

        let replayed = replay(&path, session_id).expect("replay");
        assert_eq!(replayed.events.len(), 1);
        match &replayed.events[0].data {
            EventData::ReasonCompleted(data) => {
                assert_eq!(
                    data.text_preview.as_deref(),
                    Some("Will read the lib.rs file")
                );
                assert!(data.has_tool_calls);
                assert_eq!(data.tool_call_count, 1);
            }
            other => panic!("expected ReasonCompleted, got {other:?}"),
        }
        // reason.completed is not a conversation message — it must not
        // pollute the replayed messages vec.
        assert!(replayed.messages.is_empty());
    }

    #[tokio::test]
    async fn reason_item_event_is_persisted_and_replayed() {
        use everruns_core::ReasonItemData;
        use everruns_provider::typed_id::TurnId;

        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(4823);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        let req = EventRequest::new(
            session_id,
            EventContext::default(),
            ReasonItemData {
                turn_id: TurnId::new(),
                provider: "openai".to_string(),
                model: Some("gpt-5".to_string()),
                item_id: "rs_abc123".to_string(),
                encrypted_content: Some("opaque-encrypted-blob".to_string()),
                summary: vec!["Considered file structure.".to_string()],
                token_count: Some(42),
            },
        );
        emitter.emit(req).await.expect("emit");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("\"reason.item\""),
            "reason.item should be persisted: {on_disk}"
        );
        assert!(
            on_disk.contains("opaque-encrypted-blob"),
            "encrypted_content must round-trip for provider continuation: {on_disk}"
        );

        let replayed = replay(&path, session_id).expect("replay");
        assert_eq!(replayed.events.len(), 1);
        match &replayed.events[0].data {
            EventData::ReasonItem(data) => {
                assert_eq!(data.provider, "openai");
                assert_eq!(data.item_id, "rs_abc123");
                assert_eq!(
                    data.encrypted_content.as_deref(),
                    Some("opaque-encrypted-blob")
                );
                assert_eq!(data.summary, vec!["Considered file structure.".to_string()]);
            }
            other => panic!("expected ReasonItem, got {other:?}"),
        }
        assert!(replayed.messages.is_empty());
    }

    #[tokio::test]
    async fn subscribe_receives_emitted_events_including_deltas() {
        use everruns_core::{
            OUTPUT_MESSAGE_DELTA, OutputMessageDeltaData, TOOL_OUTPUT_DELTA, ToolOutputDeltaData,
        };
        use everruns_provider::typed_id::{MessageId, TurnId};

        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(99);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        let mut rx = emitter.subscribe();

        let turn_id = TurnId::new();
        let delta_req = EventRequest::new(
            session_id,
            EventContext::default(),
            OutputMessageDeltaData {
                turn_id,
                message_id: MessageId::new(),
                delta: "hello".to_string(),
                accumulated: "hello".to_string(),
                phase: None,
            },
        );
        let _ = emitter.emit(delta_req).await.expect("emit delta");

        let tool_req = EventRequest::new(
            session_id,
            EventContext::default(),
            ToolOutputDeltaData {
                tool_call_id: "call-1".to_string(),
                tool_name: "bash".to_string(),
                delta: "running...\n".to_string(),
                stream: "stdout".to_string(),
            },
        );
        let _ = emitter.emit(tool_req).await.expect("emit tool delta");

        let first = rx.recv().await.expect("first event");
        let second = rx.recv().await.expect("second event");
        assert_eq!(first.event_type, OUTPUT_MESSAGE_DELTA);
        assert_eq!(second.event_type, TOOL_OUTPUT_DELTA);

        // Streaming events should not materialize the JSONL file.
        assert!(
            !path.exists(),
            "delta-only activity must not create a session log"
        );
    }

    #[tokio::test]
    async fn subscribe_does_not_replay_seeded_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session_id = SessionId::from_seed(101);
        let session_dir = session_dir_path(dir.path(), session_id);
        let path = session_log_path(&session_dir);
        let emitter = JsonlEventEmitter::open(&path, 1).expect("open");

        emitter
            .seed_replayed(vec![input_event(session_id, "old turn")])
            .await;
        let mut rx = emitter.subscribe();

        // Without any new emissions, recv() must time out — the broadcast
        // is post-emission only, never replays seeded history.
        let drained = tokio::time::timeout(std::time::Duration::from_millis(40), rx.recv()).await;
        assert!(
            drained.is_err(),
            "no event should be delivered, got {drained:?}"
        );
    }

    #[test]
    fn tool_completed_replay_preserves_json_result_shape() {
        let data = ToolCompletedData::success(
            "call_read".to_string(),
            "read_file".to_string(),
            vec![ContentPart::text(
                serde_json::json!({
                    "path": "/repo/src/lib.rs",
                    "content": "1|fn main() {}"
                })
                .to_string(),
            )],
            Some(1),
        );

        let message = tool_completed_to_message(data);
        let result = message
            .tool_result_content()
            .and_then(|content| content.result.as_ref())
            .expect("tool result should be present");

        assert_eq!(result["path"], "/repo/src/lib.rs");
        assert_eq!(result["content"], "1|fn main() {}");
    }

    #[test]
    fn tool_completed_replay_keeps_scalar_json_as_text() {
        let data = ToolCompletedData::success(
            "call_scalar".to_string(),
            "custom_tool".to_string(),
            vec![ContentPart::text("123")],
            Some(1),
        );

        let message = tool_completed_to_message(data);
        let result = message
            .tool_result_content()
            .and_then(|content| content.result.as_ref())
            .expect("tool result should be present");

        assert_eq!(result, &serde_json::Value::String("123".to_string()));
    }

    // ====================================================================
    // replay() edge cases — partial writes, malformed lines, foreign
    // session ids, binary garbage. The contract is:
    //   * never panic
    //   * skip invalid content with a tracing warning
    //   * preserve every valid Event belonging to `expected`
    //   * track max_sequence across all valid events
    // ====================================================================

    fn serialize_event(event: &Event) -> String {
        serde_json::to_string(event).expect("serialize event")
    }

    #[test]
    fn replay_missing_file_returns_empty_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.jsonl");
        let session_id = SessionId::from_seed(70001);

        let out = replay(&path, session_id).expect("replay missing file");
        assert!(out.events.is_empty());
        assert!(out.messages.is_empty());
        assert!(out.max_sequence.is_none());
    }

    #[test]
    fn replay_empty_file_returns_empty_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, b"").expect("write empty file");
        let session_id = SessionId::from_seed(70002);

        let out = replay(&path, session_id).expect("replay empty file");
        assert!(out.events.is_empty());
        assert!(out.messages.is_empty());
        assert!(out.max_sequence.is_none());
    }

    #[test]
    fn replay_blank_lines_in_middle_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let session_id = SessionId::from_seed(70003);

        let first = serialize_event(&input_event(session_id, "first"));
        let second = serialize_event(&input_event(session_id, "second"));
        let content = format!("{first}\n\n   \n{second}\n");
        std::fs::write(&path, content).expect("write");

        let out = replay(&path, session_id).expect("replay");
        assert_eq!(
            out.events.len(),
            2,
            "blank/whitespace-only lines must not produce events"
        );
    }

    #[test]
    fn replay_malformed_json_line_does_not_stop_replay() {
        // A single bad line in the middle must be skipped while the surrounding
        // valid lines still come back. This is the documented graceful-degrade
        // behaviour for partially-corrupt logs.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let session_id = SessionId::from_seed(70004);

        let good_a = serialize_event(&input_event(session_id, "alpha"));
        let good_b = serialize_event(&input_event(session_id, "beta"));
        let content = format!("{good_a}\n{{not valid json\n{good_b}\n");
        std::fs::write(&path, content).expect("write");

        let out = replay(&path, session_id).expect("replay");
        assert_eq!(out.events.len(), 2, "two good lines must survive");
    }

    #[test]
    fn replay_skips_events_for_different_session_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let expected = SessionId::from_seed(70005);
        let other = SessionId::from_seed(70006);

        let mine = serialize_event(&input_event(expected, "mine"));
        let theirs = serialize_event(&input_event(other, "theirs"));
        let content = format!("{theirs}\n{mine}\n{theirs}\n");
        std::fs::write(&path, content).expect("write");

        let out = replay(&path, expected).expect("replay");
        assert_eq!(
            out.events.len(),
            1,
            "only the line belonging to `expected` should be kept"
        );
        assert_eq!(out.events[0].session_id, expected);
    }

    #[test]
    fn replay_truncated_last_line_is_skipped_without_dropping_prior() {
        // Simulates a crash mid-write: the last line is a partial JSON
        // object with no trailing newline. Replay must skip it and keep
        // the prior fully-written events.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let session_id = SessionId::from_seed(70007);

        let good = serialize_event(&input_event(session_id, "kept"));
        let partial = "{\"id\":\"evt_\",\"session";
        let content = format!("{good}\n{partial}");
        std::fs::write(&path, content).expect("write");

        let out = replay(&path, session_id).expect("replay");
        assert_eq!(
            out.events.len(),
            1,
            "the partial tail must not corrupt the replay"
        );
    }

    #[test]
    fn replay_binary_garbage_stops_early_and_drops_suffix() {
        // Non-UTF-8 bytes injected into the JSONL file. `BufRead::lines()`
        // will yield Err for non-UTF-8; the documented contract is to stop
        // replay early on read errors rather than panic. A valid event is
        // sandwiched AFTER the garbage so this test fails if replay ever
        // starts trying to continue past a read error.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let session_id = SessionId::from_seed(70008);

        let prefix = serialize_event(&input_event(session_id, "prefix"));
        let suffix = serialize_event(&input_event(session_id, "would-be-suffix"));
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(prefix.as_bytes());
        bytes.push(b'\n');
        // Invalid UTF-8 sequence on its own line.
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd, b'\n']);
        bytes.extend_from_slice(suffix.as_bytes());
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).expect("write");

        let out = replay(&path, session_id).expect("replay must not panic");
        assert_eq!(
            out.events.len(),
            1,
            "only the valid prefix line should survive; replay must stop early on read error"
        );
        let kept_text = match &out.events[0].data {
            EventData::InputMessage(d) => d.message.text().unwrap_or_default().to_string(),
            other => panic!("expected InputMessage, got {other:?}"),
        };
        assert!(
            kept_text.contains("prefix"),
            "expected the prefix event to be the one kept, got: {kept_text}"
        );
    }

    #[test]
    fn replay_tracks_highest_sequence_across_all_valid_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let session_id = SessionId::from_seed(70009);

        // Manually construct events with explicit sequence numbers so we
        // exercise the max-tracking branch even when sequences are not
        // monotonically written.
        let mut a = input_event(session_id, "a");
        a.sequence = Some(7);
        let mut b = input_event(session_id, "b");
        b.sequence = Some(3);
        let mut c = input_event(session_id, "c");
        c.sequence = Some(12);

        let content = format!(
            "{}\n{}\n{}\n",
            serialize_event(&a),
            serialize_event(&b),
            serialize_event(&c)
        );
        std::fs::write(&path, content).expect("write");

        let out = replay(&path, session_id).expect("replay");
        assert_eq!(out.events.len(), 3);
        assert_eq!(out.max_sequence, Some(12));
    }

    #[test]
    fn replay_only_blank_lines_returns_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "\n\n   \n\t\n").expect("write");
        let session_id = SessionId::from_seed(70010);

        let out = replay(&path, session_id).expect("replay");
        assert!(out.events.is_empty());
        assert!(out.max_sequence.is_none());
    }
}

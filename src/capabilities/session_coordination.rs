//! Messaging between independently hosted Yolop sessions, and the
//! out-of-process thread host for the everruns `coordination` capability.
//!
//! Every session holds a presence lease file and drains a durable inbox
//! directory under its own session folder, so any session (or a shell outside
//! one) can write to any other: `yolop sessions send` is the generic form. The
//! coordination model itself is everruns': a coordinator starts threads, each
//! assignment is a session task it owns, and threads report through the
//! worker tools. What Yolop adds is where a thread runs. Upstream runs threads
//! inside the coordinator's process; here each thread is its own headless
//! `yolop --serve` process, so a thread survives its coordinator's restart and
//! a person can open it like any other session.
//!
//! Decision: the inbox is the only cross-process channel. A brief or relay for
//!   a thread, a thread's update for its coordinator, and a plain message all
//!   travel as inbox files in the target's folder, acknowledged only when the
//!   turn they woke finishes. A host that dies first leaves them pending for
//!   its next incarnation. Threads are per-coordinator files no other process
//!   writes; senders only ever create files, so coordinators never contend on
//!   a shared database lock.
//! Decision: a thread is recognized the first time its coordinator messages it
//!   (it has an `assignment` task linking it) and recorded in `yolop_threads`,
//!   so the routing survives the coordinator's in-memory session catalog.
//! Decision: the coordinator supervises its threads. A thread with pending
//!   messages and no live host is (re)started, at most once per backoff, so a
//!   crashed or idle-exited thread comes back when there is work for it.
//! Decision: thread turns are settled with everruns' `settle_thread_turn`, the
//!   rule the server and the Framework apply, only for turns an inbox delivery
//!   started. A turn the person starts in an opened thread is not settled. A
//!   turn that ends while the thread's own background work still runs is not
//!   "ended without completing": the thread is waiting on that work, and the
//!   completion wake continues it.
//! Decision: a headless host yields its session when someone opens it
//!   (`yolop --session <id>`): the opener asks through the presence row, the
//!   host exits after its current turn, and the opener takes the lock. Inbox
//!   delivery continues in the opened session.
//! Decision: a headless thread's sandbox escalation is put to its
//!   coordinator's host, which asks the person in its own approval prompt and
//!   writes the answer back; the thread's gate applies it, so "for this
//!   session" means the thread's session. A coordinator with no prompt surface
//!   (ACP, `--print`) denies, as it does for itself. With no coordinator
//!   running the request waits, up to a limit; opening the thread denies it so
//!   the hand-over is not stuck behind it, and the person can retry there.
//! Decision: a thread whose host vanished mid-assignment (crash, kill, reboot)
//!   is restarted with a note to check what survived, since its background
//!   processes may not have.

use crate::runtime::background_wake::{WakeMessage, WakeSender};
use crate::sandbox_approval::{
    ApprovalDecision, ApprovalGate, ApprovalReceiver, ApprovalRequest, ApprovalScope,
};
use everruns_capabilities::capabilities::coordination::{
    THREAD_RESOLVED_DETAIL, ThreadTurn, settle_thread_turn,
};
use everruns_contracts::typed_id::SessionId;
use everruns_core::session_task::{SessionTaskFilter, TASK_KIND_ASSIGNMENT};
use everruns_core::{
    SessionTask, SessionTaskRegistry, TaskTransition, TaskTransitionObserver, wake_text_for,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const INBOX_INTERVAL: Duration = Duration::from_millis(350);
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(5);
const PRESENCE_LEASE_MS: i64 = 15_000;
/// A thread host is started at most once per this window, so a host that
/// crashes on startup is not respawned in a tight loop.
const THREAD_SPAWN_BACKOFF_MS: i64 = 30_000;
/// Attempts a plain message gets before it is given up on. Thread messages are
/// not retried: a failed thread turn fails the assignment, and the coordinator
/// decides what happens next.
const MAX_DELIVERY_ATTEMPTS: i64 = 3;
const DELIVERY_RETRY_BACKOFF_MS: i64 = 30_000;
/// What a thread restarted mid-assignment is told first.
const RESUME_NOTE: &str = "[automatic] This thread's process stopped while it was working on its assignment, and it has been restarted. Commands it was running were stopped, so builds, tests or servers it had started need starting again. Check where the work stands, then continue it or report with complete_assignment.";
/// Host-only input metadata naming the inbox message a wake turn delivers.
pub(crate) const DELIVERY_METADATA_KEY: &str = "yolop.coordination_delivery";
pub(crate) const MAX_MESSAGE_BYTES: usize = 32 * 1024;
/// How often a thread host checks for answers to its relayed escalations.
const APPROVAL_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long a relayed escalation waits for an answer before it is denied.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A thread's sandbox escalation waiting on its coordinator.
#[derive(Debug)]
struct ThreadApproval {
    approval_id: String,
    thread_id: SessionId,
    request: ApprovalRequest,
}

/// Sandbox approvals a session host handles for coordination.
pub(crate) struct HostApprovals {
    /// This host's own gate. A coordinator asks the person through it on
    /// behalf of its threads.
    pub(crate) gate: Arc<ApprovalGate>,
    /// A headless thread's own escalations, to relay to its coordinator.
    pub(crate) relay: Option<ApprovalReceiver>,
}

/// An escalation this thread host relayed and is waiting on.
struct RelayedApproval {
    approval_id: String,
    request_digest: [u8; 32],
    reply: tokio::sync::oneshot::Sender<ApprovalDecision>,
    deadline: tokio::time::Instant,
}
/// Launch flags a thread host inherits from the process that started it.
/// `--sandbox` is a containment override: without it a coordinator started
/// with `--sandbox` (WorkspaceWrite) would spawn a DangerFullAccess child.
const INHERITED_FLAGS: [&str; 9] = [
    "--profile",
    "--provider",
    "--model",
    "--reasoning-effort",
    "--config-dir",
    "--data-dir",
    "--session-dir",
    "--env-context",
    "--sandbox",
];
/// Inherited flags that take no value; `--sandbox` is a boolean override.
const INHERITED_BOOL_FLAGS: [&str; 1] = ["--sandbox"];

/// What one inbox row carries.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum InboxPayload {
    /// A person, or another session, wrote to this session.
    Message { from: Option<String>, text: String },
    /// A coordinator's brief or relay to one of its threads, already framed by
    /// the coordination capability.
    Thread { text: String },
    /// An assignment this session coordinates changed in a thread's process.
    TaskUpdate { text: String },
}

impl InboxPayload {
    fn prompt(&self) -> String {
        match self {
            Self::Message {
                from: Some(from),
                text,
            } => format!(
                "Message from session {from}:\n\n{text}\n\nAnswer with `yolop sessions send {from} \"...\"` when it needs a reply."
            ),
            Self::Message { from: None, text } => {
                format!("Message sent to this session from a shell:\n\n{text}")
            }
            Self::Thread { text } => text.clone(),
            Self::TaskUpdate { text } => format!(
                "[automatic] Update from a thread:\n\n{text}\n\nThis is an automatic update, not a new request from the person. Read the thread with get_thread before acting on it."
            ),
        }
    }

    fn is_thread(&self) -> bool {
        matches!(self, Self::Thread { .. })
    }
}

/// A coordinator's thread, as the transport needs to know it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadRecord {
    pub(crate) thread_id: SessionId,
    pub(crate) coordinator: SessionId,
    /// Where the thread's host starts; the thread may move into a worktree.
    pub(crate) workspace_root: PathBuf,
    /// Flags the thread's host is launched with (profile, provider, ...).
    pub(crate) launch: Vec<String>,
}

/// Live presence of one session.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct LiveSession {
    pub(crate) session_id: String,
    pub(crate) workspace_root: String,
    pub(crate) thread_of: Option<String>,
}

/// Where coordination state lives now that the shared SQLite database is
/// gone. Every record is a file with a single owning writer:
///
/// - `<sessions_dir>/<session>/presence.json`: written only by that
///   session's host (its heartbeat lease). Replaces the old
///   `yolop_session_presence` table and its every-2-seconds cross-process
///   writes, which were the main source of `database is locked` contention.
/// - `<sessions_dir>/<session>/inbox/<message>.json`: the fanout. A sender
///   drops a uniquely-named file into the target's inbox and never touches
///   anyone else's state. Claiming is an atomic rename to
///   `<message>.claimed-<host>.json`, so exactly one host delivers each
///   message even when two processes poll the same inbox.
/// - `<sessions_dir>/coordination/threads/<thread>.json`: owned by the
///   thread's coordinator; only that coordinator records or respawns it.
/// - `<sessions_dir>/coordination/approvals/<id>.json`: filed by a thread
///   host, decided by its coordinator.
///
/// Readers scan directories; writers touch only their own paths (plus atomic
/// file creation into a target's inbox). No shared database, so no
/// single-writer lock for coordinators to collide on.
#[derive(Clone)]
pub(crate) struct CoordinationStore {
    sessions_dir: PathBuf,
    /// Holds the backing directory for ephemeral stores (tests, fallbacks
    /// with no sessions dir), the file equivalent of the old in-memory
    /// SQLite database.
    _temp: Option<std::sync::Arc<tempfile::TempDir>>,
}

/// A session host's heartbeat lease. Owned by the named session's host.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PresenceFile {
    host_id: String,
    project_id: String,
    workspace_root: String,
    heartbeat_at_ms: i64,
    headless: bool,
    #[serde(default)]
    release_requested_at_ms: i64,
}

/// One inbox message: a pending fanout delivery or an in-flight claim.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct InboxFile {
    message_id: String,
    source_session_id: Option<String>,
    payload: InboxPayload,
    created_at_ms: i64,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    available_at_ms: i64,
    #[serde(default)]
    delivered_host_id: Option<String>,
}

/// A coordinator's thread record. Only the owning coordinator writes it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThreadFile {
    thread_id: String,
    coordinator: String,
    workspace_root: String,
    launch: Vec<String>,
    created_at_ms: i64,
    #[serde(default)]
    spawned_at_ms: i64,
}

/// A thread host's escalation waiting on (or decided by) its coordinator.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ApprovalFile {
    approval_id: String,
    thread_id: String,
    coordinator: String,
    host_id: String,
    command: String,
    reason: String,
    scope: String,
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    request_digest: Option<Vec<u8>>,
    created_at_ms: i64,
}

/// Process-wide counter so generated file names are unique without a
/// database sequence.
static FILE_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unique_suffix() -> String {
    let n = FILE_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{}-{n}", now_ms(), std::process::id())
}

/// Write `bytes` to `path` atomically: readers never see a half-written
/// record, and concurrent writers to different paths never block.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", unique_suffix()));
    let wrote = (|| -> anyhow::Result<()> {
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if wrote.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    wrote
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_atomic(path, &bytes)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
    }
}

/// Create `path` with `bytes`, failing when it already exists. The
/// create-new open is atomic, so this is the mutual-exclusion primitive for
/// spawn claims; inbox claims use atomic rename instead.
fn create_new(path: &Path, bytes: &[u8]) -> anyhow::Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            use std::io::Write;
            let wrote = file.write_all(bytes).and_then(|()| file.sync_all());
            if wrote.is_err() {
                let _ = std::fs::remove_file(path);
            }
            wrote?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    }
}

impl CoordinationStore {
    /// Open the file-backed store at `sessions_dir`, importing the legacy
    /// shared database on first open when one is present.
    pub(crate) fn open(sessions_dir: &Path) -> anyhow::Result<Self> {
        migrate_legacy_db(sessions_dir);
        Ok(Self {
            sessions_dir: sessions_dir.to_path_buf(),
            _temp: None,
        })
    }

    /// An ephemeral store backed by a fresh temporary directory: the file
    /// equivalent of the old in-memory SQLite database, for tests and for
    /// processes with no sessions dir.
    pub(crate) fn ephemeral() -> anyhow::Result<Self> {
        let temp = tempfile::TempDir::new()?;
        Ok(Self {
            sessions_dir: temp.path().to_path_buf(),
            _temp: Some(std::sync::Arc::new(temp)),
        })
    }

    fn coordination_dir(&self) -> PathBuf {
        self.sessions_dir.join("coordination")
    }

    fn threads_dir(&self) -> PathBuf {
        self.coordination_dir().join("threads")
    }

    fn approvals_dir(&self) -> PathBuf {
        self.coordination_dir().join("approvals")
    }

    fn presence_dir(&self) -> PathBuf {
        self.coordination_dir().join("presence")
    }

    fn presence_path(&self, session_id: &str) -> PathBuf {
        self.presence_dir().join(format!("{session_id}.json"))
    }

    fn inbox_dir(&self, session_id: &str) -> PathBuf {
        self.coordination_dir().join("inbox").join(session_id)
    }

    fn thread_path(&self, thread_id: &str) -> PathBuf {
        self.threads_dir().join(format!("{thread_id}.json"))
    }

    fn approval_path(&self, approval_id: &str) -> PathBuf {
        self.approvals_dir().join(format!("{approval_id}.json"))
    }

    fn read_presence(&self, session_id: &str) -> anyhow::Result<Option<PresenceFile>> {
        read_json(&self.presence_path(session_id))
    }

    fn presence_is_live(presence: Option<&PresenceFile>) -> bool {
        match presence {
            Some(seen) => {
                seen.heartbeat_at_ms > 0 && seen.heartbeat_at_ms > now_ms() - PRESENCE_LEASE_MS
            }
            None => false,
        }
    }
    /// Every presence lease, newest first.
    fn all_presence(&self) -> anyhow::Result<Vec<(String, PresenceFile)>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(self.presence_dir()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(error.into()),
            Ok(entries) => entries,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(presence) = read_json::<PresenceFile>(&path)? {
                out.push((name.to_string(), presence));
            }
        }
        out.sort_by_key(|(_, presence)| std::cmp::Reverse(presence.heartbeat_at_ms));
        Ok(out)
    }

    /// All thread records. Threads are owned by their coordinator (only it
    /// writes them) but kept in one directory so any process can resolve a
    /// thread to its coordinator without a shared database.
    fn all_threads(&self) -> anyhow::Result<Vec<ThreadFile>> {
        let mut threads = Vec::new();
        let entries = match std::fs::read_dir(self.threads_dir()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(threads),
            Err(error) => return Err(error.into()),
            Ok(entries) => entries,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(thread) = read_json::<ThreadFile>(&path)? {
                threads.push(thread);
            }
        }
        Ok(threads)
    }

    /// Pending (unclaimed, due) inbox messages for one session, oldest first.
    /// This is the read side of the fanout; senders only ever create files.
    fn pending_inbox(&self, session_id: &str) -> anyhow::Result<Vec<(PathBuf, InboxFile)>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(self.inbox_dir(session_id)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(error.into()),
            Ok(entries) => entries,
        };
        let now = now_ms();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".json") || name.contains(".claimed-") {
                continue;
            }
            let Some(message) = read_json::<InboxFile>(&path)? else {
                continue;
            };
            if message.available_at_ms <= now {
                out.push((path, message));
            }
        }
        out.sort_by_key(|(_, message)| message.created_at_ms);
        Ok(out)
    }

    /// Whether `host_id` currently holds a fresh lease anywhere. Tells a
    /// live claim holder apart from a dead one when reoffering deliveries.
    fn host_is_live(&self, host_id: &str) -> anyhow::Result<bool> {
        for (_, presence) in self.all_presence()? {
            if presence.host_id == host_id && Self::presence_is_live(Some(&presence)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether `session_id` has any unacked inbox file, pending or claimed.
    /// Crash restarts ignore backoff: a claimed message still needs a host.
    fn has_unacked_inbox(&self, session_id: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(self.inbox_dir(session_id)) else {
            return false;
        };
        entries
            .flatten()
            .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
    }

    /// The claim file for `message_id` in any inbox, if one exists.
    fn find_claim(&self, message_id: &str) -> anyhow::Result<Option<PathBuf>> {
        let inboxes = self.coordination_dir().join("inbox");
        let entries = match std::fs::read_dir(&inboxes) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(entries) => entries,
        };
        for entry in entries.flatten() {
            let inbox = entry.path();
            if !inbox.is_dir() {
                continue;
            }
            let Ok(claims) = std::fs::read_dir(&inbox) else {
                continue;
            };
            for claim in claims.flatten() {
                let path = claim.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if name.starts_with(message_id) && name.contains(".claimed-") {
                    return Ok(Some(path));
                }
            }
        }
        Ok(None)
    }

    fn register_host(
        &self,
        session_id: SessionId,
        host_id: &str,
        project_id: &str,
        workspace_root: &Path,
        headless: bool,
    ) -> anyhow::Result<()> {
        let presence = PresenceFile {
            host_id: host_id.to_string(),
            project_id: project_id.to_string(),
            workspace_root: workspace_root.to_string_lossy().into_owned(),
            heartbeat_at_ms: now_ms(),
            headless,
            release_requested_at_ms: 0,
        };
        write_json(&self.presence_path(&session_id.to_string()), &presence)
    }

    fn heartbeat(&self, session_id: SessionId, host_id: &str) -> anyhow::Result<()> {
        let path = self.presence_path(&session_id.to_string());
        let Some(mut presence) = read_json::<PresenceFile>(&path)? else {
            return Ok(());
        };
        if presence.host_id != host_id {
            return Ok(());
        }
        presence.heartbeat_at_ms = now_ms();
        write_json(&path, &presence)
    }

    fn mark_offline(&self, session_id: SessionId, host_id: &str) {
        let path = self.presence_path(&session_id.to_string());
        let Ok(Some(mut presence)) = read_json::<PresenceFile>(&path) else {
            return;
        };
        if presence.host_id != host_id {
            return;
        }
        presence.heartbeat_at_ms = 0;
        let _ = write_json(&path, &presence);
    }

    pub(crate) fn is_live(&self, session_id: SessionId) -> anyhow::Result<bool> {
        Ok(Self::presence_is_live(
            self.read_presence(&session_id.to_string())?.as_ref(),
        ))
    }

    pub(crate) fn request_release(&self, session_id: SessionId) -> anyhow::Result<bool> {
        let path = self.presence_path(&session_id.to_string());
        let Some(mut presence) = read_json::<PresenceFile>(&path)? else {
            return Ok(false);
        };
        if !presence.headless || !Self::presence_is_live(Some(&presence)) {
            return Ok(false);
        }
        presence.release_requested_at_ms = now_ms();
        write_json(&path, &presence)?;
        Ok(true)
    }

    /// Ask a live headless host to release the session, waiting until it
    /// goes offline. Keeps deferring the supervisor's spawn backoff while
    /// waiting, so the handover is not raced by a fresh host.
    pub(crate) async fn take_over(
        &self,
        session_id: SessionId,
        timeout: std::time::Duration,
    ) -> anyhow::Result<bool> {
        if !self.request_release(session_id)? {
            return Ok(true);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.defer_spawn(session_id)?;
            if !self.is_live(session_id)? {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
        }
    }

    /// Restart the backoff on a thread's host spawn, so the coordinator does
    /// not start a headless host for a session a person is opening.
    fn defer_spawn(&self, thread_id: SessionId) -> anyhow::Result<()> {
        let path = self.thread_path(&thread_id.to_string());
        let Some(mut thread) = read_json::<ThreadFile>(&path)? else {
            return Ok(());
        };
        thread.spawned_at_ms = now_ms();
        write_json(&path, &thread)
    }

    fn release_requested(&self, session_id: SessionId, host_id: &str) -> anyhow::Result<bool> {
        let presence = self.read_presence(&session_id.to_string())?;
        Ok(
            matches!(presence, Some(seen) if seen.host_id == host_id && seen.release_requested_at_ms > 0),
        )
    }
    /// Put a thread host's sandbox escalation to its coordinator.
    /// Requests an earlier host of the thread left behind are dropped.
    fn request_approval(
        &self,
        thread_id: SessionId,
        coordinator: SessionId,
        host_id: &str,
        request: &ApprovalRequest,
    ) -> anyhow::Result<String> {
        request.validate()?;
        let digest = request.digest();
        if let Ok(entries) = std::fs::read_dir(self.approvals_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(Some(approval)) = read_json::<ApprovalFile>(&path) else {
                    continue;
                };
                if approval.thread_id == thread_id.to_string() && approval.host_id != host_id {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let approval_id = format!("appr-{}", unique_suffix());
        let approval = ApprovalFile {
            approval_id: approval_id.clone(),
            thread_id: thread_id.to_string(),
            coordinator: coordinator.to_string(),
            host_id: host_id.to_string(),
            command: request.command.clone(),
            reason: request.reason.clone(),
            scope: request.scope.as_str().to_string(),
            decision: None,
            request_digest: Some(digest.to_vec()),
            created_at_ms: now_ms(),
        };
        write_json(&self.approval_path(&approval_id), &approval)?;
        Ok(approval_id)
    }

    /// Read a relayed decision. `None` means no answer yet; an unknown scope
    /// or a digest mismatch fails closed to deny, as before.
    fn approval_answer(
        &self,
        approval_id: &str,
        digest: &[u8; 32],
    ) -> anyhow::Result<Option<ApprovalDecision>> {
        let Some(approval) = read_json::<ApprovalFile>(&self.approval_path(approval_id))? else {
            return Ok(None);
        };
        let Some(scope) = ApprovalScope::parse(&approval.scope) else {
            return Ok(Some(ApprovalDecision::Deny));
        };
        let request = ApprovalRequest {
            command: approval.command.clone(),
            reason: approval.reason.clone(),
            scope,
        };
        if !request.matches_digest(approval.request_digest.as_deref())
            || approval.request_digest.as_deref() != Some(digest.as_slice())
        {
            return Ok(Some(ApprovalDecision::Deny));
        }
        Ok(approval
            .decision
            .as_deref()
            .and_then(ApprovalDecision::parse))
    }

    fn drop_approval(&self, approval_id: &str) {
        let _ = std::fs::remove_file(self.approval_path(approval_id));
    }

    /// Pending escalations for `coordinator`, oldest first. Stale ones
    /// (unknown scope, invalid request, dead thread host) are denied or
    /// dropped instead of presented, as before.
    fn pending_approvals(&self, coordinator: SessionId) -> anyhow::Result<Vec<ThreadApproval>> {
        let mut approvals = Vec::new();
        let entries = match std::fs::read_dir(self.approvals_dir()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(approvals);
            }
            Err(error) => return Err(error.into()),
            Ok(entries) => entries,
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        files.sort();
        for path in files {
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(approval) = read_json::<ApprovalFile>(&path)? else {
                continue;
            };
            if approval.coordinator != coordinator.to_string() || approval.decision.is_some() {
                continue;
            }
            let Some(scope) = ApprovalScope::parse(&approval.scope) else {
                let _ = self.answer_approval(&approval.approval_id, ApprovalDecision::Deny);
                continue;
            };
            let request = ApprovalRequest {
                command: approval.command.clone(),
                reason: approval.reason.clone(),
                scope,
            };
            if !request.matches_digest(approval.request_digest.as_deref()) {
                let _ = self.answer_approval(&approval.approval_id, ApprovalDecision::Deny);
                continue;
            }
            let live = self
                .read_presence(&approval.thread_id)?
                .map(|seen| seen.host_id == approval.host_id && Self::presence_is_live(Some(&seen)))
                .unwrap_or(false);
            if !live {
                continue;
            }
            let thread_id: SessionId = approval.thread_id.parse()?;
            approvals.push(ThreadApproval {
                approval_id: approval.approval_id,
                thread_id,
                request,
            });
        }
        Ok(approvals)
    }

    fn answer_approval(&self, approval_id: &str, decision: ApprovalDecision) -> anyhow::Result<()> {
        let path = self.approval_path(approval_id);
        let Some(mut approval) = read_json::<ApprovalFile>(&path)? else {
            return Ok(());
        };
        if approval.decision.is_none() {
            approval.decision = Some(decision.as_str().to_string());
            write_json(&path, &approval)?;
        }
        Ok(())
    }

    /// Live sessions, newest heartbeat first, with the coordinator that owns
    /// each thread. Presence comes from per-session leases, thread ownership
    /// from the coordinator-owned thread files.
    pub(crate) fn live_sessions(&self) -> anyhow::Result<Vec<LiveSession>> {
        let threads: std::collections::HashMap<String, String> = self
            .all_threads()?
            .into_iter()
            .map(|thread| (thread.thread_id, thread.coordinator))
            .collect();
        let mut sessions = Vec::new();
        for (session_id, presence) in self.all_presence()? {
            if !Self::presence_is_live(Some(&presence)) {
                continue;
            }
            sessions.push(LiveSession {
                thread_of: threads.get(&session_id).cloned(),
                workspace_root: presence.workspace_root,
                session_id,
            });
        }
        Ok(sessions)
    }

    /// Fan out one message into the target's inbox. The sender only creates
    /// a uniquely-named file, so senders never contend with each other or
    /// with the target's host.
    pub(crate) fn enqueue(
        &self,
        target: SessionId,
        source: Option<SessionId>,
        payload: &InboxPayload,
    ) -> anyhow::Result<String> {
        let rendered = serde_json::to_string(payload)?;
        anyhow::ensure!(
            rendered.len() <= MAX_MESSAGE_BYTES,
            "coordination message is {} bytes, over the {MAX_MESSAGE_BYTES} byte limit",
            rendered.len()
        );
        let dir = self.inbox_dir(&target.to_string());
        std::fs::create_dir_all(&dir)?;
        for _ in 0..3 {
            let message_id = new_message_id();
            let message = InboxFile {
                message_id: message_id.clone(),
                source_session_id: source.map(|session| session.to_string()),
                payload: payload.clone(),
                created_at_ms: now_ms(),
                attempts: 0,
                available_at_ms: 0,
                delivered_host_id: None,
            };
            let bytes = serde_json::to_vec_pretty(&message)?;
            if create_new(&dir.join(format!("{message_id}.json")), &bytes)? {
                return Ok(message_id);
            }
        }
        anyhow::bail!("could not place a message in the session inbox")
    }

    /// Claim the oldest pending message for `session_id`, or reoffer one
    /// whose claiming host died. The claim is an atomic rename, so two hosts
    /// polling the same inbox cannot deliver the same message twice.
    fn next_delivery(
        &self,
        session_id: SessionId,
        host_id: &str,
    ) -> anyhow::Result<Option<Delivery>> {
        let session = session_id.to_string();
        let dir = self.inbox_dir(&session);
        let now = now_ms();
        let mut stale: Vec<(PathBuf, String, InboxFile)> = Vec::new();
        let mut pending: Vec<(PathBuf, String, InboxFile)> = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(entries) => entries,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".json").map(str::to_owned) else {
                continue;
            };
            if let Some((base, claim_host)) = stem.split_once(".claimed-") {
                if claim_host == host_id {
                    continue;
                }
                if self.host_is_live(claim_host)? {
                    continue;
                }
                if let Some(message) = read_json::<InboxFile>(&path)? {
                    stale.push((path, base.to_string(), message));
                }
                continue;
            }
            if let Some(message) = read_json::<InboxFile>(&path)? {
                if message.available_at_ms > now {
                    continue;
                }
                if message.attempts >= MAX_DELIVERY_ATTEMPTS as u32 {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                pending.push((path, stem.to_string(), message));
            }
        }
        pending.sort_by_key(|(_, _, message)| message.created_at_ms);
        stale.sort_by_key(|(_, _, message)| message.created_at_ms);
        for (path, base, mut message) in pending.into_iter().chain(stale) {
            let claimed = dir.join(format!("{base}.claimed-{host_id}.json"));
            match std::fs::rename(&path, &claimed) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
                Ok(()) => {
                    message.attempts += 1;
                    message.delivered_host_id = Some(host_id.to_string());
                    write_json(&claimed, &message)?;
                    return Ok(Some(Delivery {
                        message_id: message.message_id,
                        payload: message.payload,
                    }));
                }
            }
        }
        Ok(None)
    }

    fn ack(&self, message_id: &str) -> anyhow::Result<()> {
        if let Some(path) = self.find_claim(message_id)? {
            match std::fs::remove_file(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
                Ok(()) => Ok(()),
            }
        } else {
            Ok(())
        }
    }

    /// Return a message to its inbox with backoff. Thread briefs are not
    /// retried: a failed thread turn fails the assignment instead. An
    /// exhausted message is dropped, as before.
    fn nack(&self, message_id: &str) -> anyhow::Result<()> {
        let Some(path) = self.find_claim(message_id)? else {
            return Ok(());
        };
        let Some(mut message) = read_json::<InboxFile>(&path)? else {
            return Ok(());
        };
        if message.payload.is_thread() {
            return self.ack(message_id);
        }
        if message.attempts >= MAX_DELIVERY_ATTEMPTS as u32 {
            return self.ack(message_id);
        }
        message.available_at_ms = now_ms() + DELIVERY_RETRY_BACKOFF_MS;
        message.delivered_host_id = None;
        write_json(&path, &message)?;
        let pending = path.with_file_name(format!("{message_id}.json"));
        match std::fs::rename(&path, &pending) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
            Ok(()) => Ok(()),
        }
    }
    /// Record a thread under its coordinator. Only the owning coordinator
    /// writes its threads; everyone else resolves them read-only.
    pub(crate) fn record_thread(&self, thread: &ThreadRecord) -> anyhow::Result<()> {
        let record = ThreadFile {
            thread_id: thread.thread_id.to_string(),
            coordinator: thread.coordinator.to_string(),
            workspace_root: thread.workspace_root.to_string_lossy().into_owned(),
            launch: thread.launch.clone(),
            created_at_ms: now_ms(),
            spawned_at_ms: 0,
        };
        let bytes = serde_json::to_vec_pretty(&record)?;
        create_new(&self.thread_path(&record.thread_id), &bytes)?;
        Ok(())
    }

    pub(crate) fn thread(&self, thread_id: SessionId) -> anyhow::Result<Option<ThreadRecord>> {
        let Some(record) = read_json::<ThreadFile>(&self.thread_path(&thread_id.to_string()))?
        else {
            return Ok(None);
        };
        Ok(Some(ThreadRecord {
            thread_id: record.thread_id.parse()?,
            coordinator: record.coordinator.parse()?,
            workspace_root: PathBuf::from(record.workspace_root),
            launch: record.launch,
        }))
    }

    pub(crate) fn has_threads(&self, coordinator: SessionId) -> anyhow::Result<bool> {
        let coordinator = coordinator.to_string();
        Ok(self
            .all_threads()?
            .iter()
            .any(|thread| thread.coordinator == coordinator))
    }

    /// Threads with pending messages, no live host, and a past spawn
    /// backoff: the coordinator's supervisor starts a host for these.
    fn threads_needing_host(&self, coordinator: SessionId) -> anyhow::Result<Vec<ThreadRecord>> {
        let coordinator = coordinator.to_string();
        let now = now_ms();
        let mut threads = Vec::new();
        for thread in self.all_threads()? {
            if thread.coordinator != coordinator {
                continue;
            }
            if !self
                .pending_inbox(&thread.thread_id)?
                .iter()
                .any(|(_, message)| message.attempts < MAX_DELIVERY_ATTEMPTS as u32)
            {
                continue;
            }
            if Self::presence_is_live(self.read_presence(&thread.thread_id)?.as_ref()) {
                continue;
            }
            if thread.spawned_at_ms > 0 && thread.spawned_at_ms > now - THREAD_SPAWN_BACKOFF_MS {
                continue;
            }
            threads.push(ThreadRecord {
                thread_id: thread.thread_id.parse()?,
                coordinator: thread.coordinator.parse()?,
                workspace_root: PathBuf::from(thread.workspace_root),
                launch: thread.launch,
            });
        }
        threads.sort_by_key(|thread| thread.thread_id.to_string());
        Ok(threads)
    }

    /// Threads with no live host and no pending messages that once spawned:
    /// their host vanished mid-assignment, so the coordinator restarts them
    /// with a note to check what survived.
    fn hostless_threads(&self, coordinator: SessionId) -> anyhow::Result<Vec<ThreadRecord>> {
        let coordinator = coordinator.to_string();
        let now = now_ms();
        let mut threads = Vec::new();
        for thread in self.all_threads()? {
            if thread.coordinator != coordinator {
                continue;
            }
            if Self::presence_is_live(self.read_presence(&thread.thread_id)?.as_ref()) {
                continue;
            }
            if self.has_unacked_inbox(&thread.thread_id) {
                continue;
            }
            if thread.spawned_at_ms > now - THREAD_SPAWN_BACKOFF_MS {
                continue;
            }
            threads.push(ThreadRecord {
                thread_id: thread.thread_id.parse()?,
                coordinator: thread.coordinator.parse()?,
                workspace_root: PathBuf::from(thread.workspace_root),
                launch: thread.launch,
            });
        }
        Ok(threads)
    }

    /// Atomically claim the right to spawn a host for `thread_id`. The
    /// create-new sentinel is the mutual exclusion: two supervisors racing
    /// for the same thread cannot both win, even across processes.
    fn claim_spawn(&self, thread_id: SessionId) -> anyhow::Result<bool> {
        let path = self.thread_path(&thread_id.to_string());
        let Some(thread) = read_json::<ThreadFile>(&path)? else {
            return Ok(false);
        };
        if thread.spawned_at_ms > 0 && thread.spawned_at_ms > now_ms() - THREAD_SPAWN_BACKOFF_MS {
            return Ok(false);
        }
        let lock = self.threads_dir().join(format!(".spawn-{thread_id}.lock"));
        let now = now_ms();
        if !create_new(&lock, format!("{}\n{now}", std::process::id()).as_bytes())? {
            let stale = std::fs::read_to_string(&lock)
                .ok()
                .and_then(|content| content.lines().nth(1)?.parse::<i64>().ok())
                .map(|claimed_at| now - claimed_at > THREAD_SPAWN_BACKOFF_MS)
                .unwrap_or(true);
            if !stale {
                return Ok(false);
            }
            let _ = std::fs::remove_file(&lock);
            if !create_new(&lock, format!("{}\n{now}", std::process::id()).as_bytes())? {
                return Ok(false);
            }
        }
        let claimed = (|| -> anyhow::Result<()> {
            let Some(mut thread) = read_json::<ThreadFile>(&path)? else {
                return Ok(());
            };
            thread.spawned_at_ms = now;
            write_json(&path, &thread)
        })();
        let _ = std::fs::remove_file(&lock);
        claimed?;
        Ok(true)
    }
}
/// Import the legacy shared coordination database
/// (`everruns-local/local.db`) into per-owner files. Runs once: after a
/// successful import the database is renamed to a `.migrated-<millis>`
/// backup and a marker records the migration, so a later open never
/// reimports. A failed import leaves the database in place and retries on
/// the next open; nothing is deleted until every row has a file.
fn migrate_legacy_db(sessions_dir: &Path) {
    let legacy = sessions_dir.join("everruns-local/local.db");
    let marker = sessions_dir.join("coordination/migrated-v1");
    if marker.exists() || !legacy.exists() {
        return;
    }
    match import_legacy_db(sessions_dir, &legacy) {
        Ok(()) => {
            let backup =
                sessions_dir.join(format!("everruns-local/local.db.migrated-{}", now_ms()));
            let _ = std::fs::rename(&legacy, &backup);
            let _ = write_atomic(&marker, b"migrated from everruns-local/local.db");
        }
        Err(error) => {
            eprintln!("yolop: coordination migration deferred, will retry: {error:#}");
        }
    }
}

fn legacy_rows(
    conn: &rusqlite::Connection,
    table: &str,
) -> anyhow::Result<Vec<std::collections::HashMap<String, rusqlite::types::Value>>> {
    let mut stmt = conn.prepare(&format!("SELECT * FROM {table}"))?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(ToString::to_string)
        .collect();
    let rows = stmt
        .query_map([], |row| {
            let mut map = std::collections::HashMap::new();
            for (index, name) in names.iter().enumerate() {
                map.insert(name.clone(), row.get::<_, rusqlite::types::Value>(index)?);
            }
            Ok(map)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn legacy_text(
    row: &std::collections::HashMap<String, rusqlite::types::Value>,
    key: &str,
) -> Option<String> {
    match row.get(key)? {
        rusqlite::types::Value::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn legacy_int(
    row: &std::collections::HashMap<String, rusqlite::types::Value>,
    key: &str,
    default: i64,
) -> i64 {
    match row.get(key) {
        Some(rusqlite::types::Value::Integer(n)) => *n,
        _ => default,
    }
}

fn legacy_blob(
    row: &std::collections::HashMap<String, rusqlite::types::Value>,
    key: &str,
) -> Option<Vec<u8>> {
    match row.get(key)? {
        rusqlite::types::Value::Blob(bytes) => Some(bytes.clone()),
        _ => None,
    }
}

fn import_legacy_db(sessions_dir: &Path, legacy: &Path) -> anyhow::Result<()> {
    let conn =
        rusqlite::Connection::open_with_flags(legacy, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let tables: std::collections::HashSet<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let store = CoordinationStore {
        sessions_dir: sessions_dir.to_path_buf(),
        _temp: None,
    };

    // Older databases predate newer columns (release flags, delivery
    // bookkeeping, verification digests); missing values fall back to
    // fail-closed defaults instead of aborting the import.
    if tables.contains("yolop_session_presence") {
        for row in legacy_rows(&conn, "yolop_session_presence")? {
            let (Some(session_id), Some(host_id)) = (
                legacy_text(&row, "session_id"),
                legacy_text(&row, "host_id"),
            ) else {
                continue;
            };
            let presence = PresenceFile {
                host_id,
                project_id: legacy_text(&row, "project_id").unwrap_or_default(),
                workspace_root: legacy_text(&row, "workspace_root").unwrap_or_default(),
                heartbeat_at_ms: legacy_int(&row, "heartbeat_at_ms", 0),
                headless: legacy_int(&row, "headless", 0) != 0,
                release_requested_at_ms: legacy_int(&row, "release_requested_at_ms", 0),
            };
            write_json(&store.presence_path(&session_id), &presence)?;
        }
    }

    if tables.contains("yolop_session_inbox") {
        for row in legacy_rows(&conn, "yolop_session_inbox")? {
            if legacy_int(&row, "acked_at_ms", 0) != 0 {
                continue;
            }
            let (Some(message_id), Some(target), Some(payload_json)) = (
                legacy_text(&row, "message_id"),
                legacy_text(&row, "target_session_id"),
                legacy_text(&row, "payload_json"),
            ) else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<InboxPayload>(&payload_json) else {
                continue;
            };
            let delivered = legacy_text(&row, "delivered_host_id");
            let message = InboxFile {
                message_id: message_id.clone(),
                source_session_id: legacy_text(&row, "source_session_id"),
                payload,
                created_at_ms: legacy_int(&row, "created_at_ms", now_ms()),
                attempts: legacy_int(&row, "attempts", 0).max(0) as u32,
                available_at_ms: legacy_int(&row, "available_at_ms", 0),
                delivered_host_id: delivered.clone(),
            };
            let bytes = serde_json::to_vec_pretty(&message)?;
            let dir = store.inbox_dir(&target);
            std::fs::create_dir_all(&dir)?;
            let name = match delivered {
                Some(host) => format!("{message_id}.claimed-{host}.json"),
                None => format!("{message_id}.json"),
            };
            create_new(&dir.join(name), &bytes)?;
        }
    }

    if tables.contains("yolop_threads") {
        for row in legacy_rows(&conn, "yolop_threads")? {
            let (Some(thread_id), Some(coordinator)) = (
                legacy_text(&row, "thread_id"),
                legacy_text(&row, "coordinator_session_id"),
            ) else {
                continue;
            };
            let launch_json = legacy_text(&row, "launch_json").unwrap_or_default();
            let Ok(launch) = serde_json::from_str::<Vec<String>>(&launch_json) else {
                continue;
            };
            let record = ThreadFile {
                thread_id: thread_id.clone(),
                coordinator,
                workspace_root: legacy_text(&row, "workspace_root").unwrap_or_default(),
                launch,
                created_at_ms: legacy_int(&row, "created_at_ms", now_ms()),
                spawned_at_ms: legacy_int(&row, "spawned_at_ms", 0).max(0),
            };
            let bytes = serde_json::to_vec_pretty(&record)?;
            create_new(&store.thread_path(&thread_id), &bytes)?;
        }
    }

    if tables.contains("yolop_thread_approvals") {
        for row in legacy_rows(&conn, "yolop_thread_approvals")? {
            let (Some(thread_id), Some(coordinator), Some(host_id)) = (
                legacy_text(&row, "thread_id"),
                legacy_text(&row, "coordinator_session_id"),
                legacy_text(&row, "host_id"),
            ) else {
                continue;
            };
            let approval_id = match row.get("approval_id") {
                Some(rusqlite::types::Value::Integer(id)) => format!("legacy-{id}"),
                _ => format!("legacy-{}", unique_suffix()),
            };
            let approval = ApprovalFile {
                approval_id: approval_id.clone(),
                thread_id,
                coordinator,
                host_id,
                command: legacy_text(&row, "command").unwrap_or_default(),
                reason: legacy_text(&row, "reason").unwrap_or_default(),
                scope: legacy_text(&row, "scope").unwrap_or_default(),
                decision: legacy_text(&row, "decision"),
                request_digest: legacy_blob(&row, "request_digest"),
                created_at_ms: legacy_int(&row, "created_at_ms", now_ms()),
            };
            write_json(&store.approval_path(&approval_id), &approval)?;
        }
    }

    Ok(())
}
/// One claimed inbox message, identified so its outcome can be settled.
#[derive(Clone, Debug)]
struct Delivery {
    message_id: String,
    payload: InboxPayload,
}

impl Delivery {
    fn wake(&self) -> WakeMessage {
        WakeMessage::coordination_delivery(self.payload.prompt(), self.message_id.clone())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn coordination_project_id(workspace_root: &Path) -> String {
    let git_common_dir = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        })
        .and_then(|path| std::fs::canonicalize(path).ok());
    if let Some(path) = git_common_dir {
        return format!("git:{}", path.display());
    }
    let path =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    format!("file:{}", path.display())
}

fn new_message_id() -> String {
    format!("cmsg_{}", SessionId::new().uuid().simple())
}

// =============================================================================
// Thread hosts
// =============================================================================

/// Starts a headless host for a thread. Injected so tests never spawn the test
/// binary.
pub(crate) type ThreadLauncher = Arc<dyn Fn(&ThreadRecord) -> anyhow::Result<()> + Send + Sync>;

/// The real launcher: this executable, detached, as `--serve --session <id>`.
pub(crate) fn process_thread_launcher() -> ThreadLauncher {
    Arc::new(|thread: &ThreadRecord| {
        let mut command = std::process::Command::new(crate::exec::self_exe::reexec_path()?);
        command
            .arg("--serve")
            .arg("--session")
            .arg(thread.thread_id.to_string())
            .args(&thread.launch)
            .current_dir(&thread.workspace_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Its own process group: a Ctrl+C in the coordinator's terminal must
        // not take its threads down with it.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        tracing::info!(thread = %thread.thread_id, pid = child.id(), "started thread host");
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    })
}

/// The launch flags from `args` a thread host should inherit.
pub(crate) fn inherited_launch_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut inherited = Vec::new();
    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        if let Some((flag, _)) = arg.split_once('=')
            && INHERITED_FLAGS.contains(&flag)
            && !INHERITED_BOOL_FLAGS.contains(&flag)
        {
            inherited.push(arg);
        } else if INHERITED_BOOL_FLAGS.contains(&arg.as_str()) {
            // Boolean containment override: inherit the flag alone without
            // consuming the following argument as its value.
            inherited.push(arg);
        } else if INHERITED_FLAGS.contains(&arg.as_str())
            && let Some(value) = args.next()
        {
            inherited.push(arg);
            inherited.push(value);
        }
    }
    inherited
}

/// Routes a coordinator's messages to threads hosted in other processes.
#[derive(Clone)]
pub(crate) struct ThreadRouting {
    store: CoordinationStore,
    launcher: ThreadLauncher,
    workspace_root: PathBuf,
    launch: Vec<String>,
    sessions_dir: PathBuf,
}

impl ThreadRouting {
    pub(crate) fn new(
        store: CoordinationStore,
        launcher: ThreadLauncher,
        workspace_root: PathBuf,
        launch: Vec<String>,
        sessions_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            launcher,
            workspace_root,
            launch,
            sessions_dir,
        }
    }

    /// The thread record for `session_id`, recording it the first time its
    /// coordinator (`parent`) has an assignment linked to it.
    pub(crate) async fn thread(
        &self,
        session_id: SessionId,
        parent: Option<SessionId>,
        tasks: Option<&dyn SessionTaskRegistry>,
    ) -> anyhow::Result<Option<ThreadRecord>> {
        if let Some(thread) = self.store.thread(session_id)? {
            return Ok(Some(thread));
        }
        let (Some(coordinator), Some(tasks)) = (parent, tasks) else {
            return Ok(None);
        };
        let linked = tasks
            .list(
                coordinator,
                Some(&SessionTaskFilter {
                    kind: Some(TASK_KIND_ASSIGNMENT.to_string()),
                    state: None,
                }),
            )
            .await?
            .iter()
            .any(|task| task.links.child_session_id == Some(session_id));
        if !linked {
            return Ok(None);
        }
        let thread = ThreadRecord {
            thread_id: session_id,
            coordinator,
            workspace_root: self.workspace_root.clone(),
            launch: self.launch.clone(),
        };
        self.store.record_thread(&thread)?;
        Ok(Some(thread))
    }

    /// Queue `text` for the thread and make sure a host will pick it up.
    pub(crate) fn deliver(&self, thread: &ThreadRecord, text: &str) -> anyhow::Result<()> {
        self.store.enqueue(
            thread.thread_id,
            Some(thread.coordinator),
            &InboxPayload::Thread {
                text: text.to_string(),
            },
        )?;
        ensure_thread_host(&self.store, &self.launcher, thread)
    }

    pub(crate) fn is_live(&self, thread: &ThreadRecord) -> bool {
        self.store.is_live(thread.thread_id).unwrap_or(false)
    }

    /// The thread's conversation, read from its session log.
    pub(crate) fn messages(
        &self,
        thread: &ThreadRecord,
    ) -> anyhow::Result<Vec<everruns_core::RuntimeMessage>> {
        use crate::runtime::session_log;
        let dir = session_log::session_dir_path(&self.sessions_dir, thread.thread_id);
        let path = session_log::session_log_path(&dir);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let replayed = session_log::replay(&path, thread.thread_id)?;
        Ok(session_log::messages_from_events(&replayed.events))
    }
}

fn ensure_thread_host(
    store: &CoordinationStore,
    launcher: &ThreadLauncher,
    thread: &ThreadRecord,
) -> anyhow::Result<()> {
    if store.is_live(thread.thread_id)? || !store.claim_spawn(thread.thread_id)? {
        return Ok(());
    }
    launcher(thread)
}

/// Wakes coordinators hosted elsewhere when a thread changes their assignment.
///
/// Installed on every session's task registry. A transition the coordinator's
/// own process makes is left to that process; one made in a thread's process
/// (a checklist, a question, a completion, a settled turn) is queued in the
/// coordinator's inbox.
pub(crate) struct CoordinatorInboxWaker {
    store: CoordinationStore,
    host_session: SessionId,
}

impl CoordinatorInboxWaker {
    pub(crate) fn new(store: CoordinationStore, host_session: SessionId) -> Self {
        Self {
            store,
            host_session,
        }
    }
}

#[async_trait::async_trait]
impl TaskTransitionObserver for CoordinatorInboxWaker {
    async fn on_transition(
        &self,
        task: &SessionTask,
        transition: TaskTransition,
    ) -> anyhow::Result<()> {
        // Resolution is the coordinator's own act; it needs no update about it.
        if task.kind != TASK_KIND_ASSIGNMENT
            || task.session_id == self.host_session
            || task.state_detail.as_deref() == Some(THREAD_RESOLVED_DETAIL)
        {
            return Ok(());
        }
        let Some(text) = wake_text_for(task, transition) else {
            return Ok(());
        };
        self.store.enqueue(
            task.session_id,
            task.links.child_session_id,
            &InboxPayload::TaskUpdate { text },
        )?;
        Ok(())
    }
}

// =============================================================================
// Session host
// =============================================================================

pub(crate) struct CoordinationHost {
    store: CoordinationStore,
    session_id: SessionId,
    host_id: String,
    task: tokio::task::JoinHandle<()>,
}

impl CoordinationHost {
    /// Register this session's lease and run its delivery loop: heartbeat,
    /// inbox delivery, settling each delivery from the outcome of the turn it
    /// woke, and keeping this session's threads hosted while they have work.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        store: CoordinationStore,
        tasks: Arc<dyn SessionTaskRegistry>,
        session_id: SessionId,
        project_id: String,
        workspace_root: PathBuf,
        wake_tx: WakeSender,
        mut events: tokio::sync::broadcast::Receiver<everruns_core::Event>,
        launcher: ThreadLauncher,
        release: Option<Arc<tokio::sync::Notify>>,
        approvals: HostApprovals,
    ) -> anyhow::Result<Self> {
        let host_id = format!("host_{}", SessionId::new().uuid().simple());
        store.register_host(
            session_id,
            &host_id,
            &project_id,
            &workspace_root,
            release.is_some(),
        )?;
        let coordinator = store.thread(session_id)?.map(|thread| thread.coordinator);
        let poll_store = store.clone();
        let poll_host_id = host_id.clone();
        let HostApprovals { gate, relay } = approvals;
        // Only a thread has someone to relay to.
        let mut relay = relay.filter(|_| coordinator.is_some());
        let asking = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let task = tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
            let mut inbox = tokio::time::interval(INBOX_INTERVAL);
            let mut supervise = tokio::time::interval(SUPERVISE_INTERVAL);
            let mut approval_poll = tokio::time::interval(APPROVAL_POLL_INTERVAL);
            let mut relayed: Vec<RelayedApproval> = Vec::new();
            let mut tracker = DeliveryTracker::default();
            let mut events_open = true;
            let mut yielding = false;
            loop {
                tokio::select! {
                    request = async {
                        match relay.as_mut() {
                            Some(relay) => relay.recv().await,
                            None => std::future::pending().await,
                        }
                    }, if relay.is_some() => {
                        let Some((request, reply)) = request else {
                            relay = None;
                            continue;
                        };
                        let Some(coordinator) = coordinator.filter(|_| !yielding) else {
                            let _ = reply.send(ApprovalDecision::Deny);
                            continue;
                        };
                        match poll_store.request_approval(session_id, coordinator, &poll_host_id, &request) {
                            Ok(approval_id) => relayed.push(RelayedApproval {
                                approval_id,
                                request_digest: request.digest(),
                                reply,
                                deadline: tokio::time::Instant::now() + APPROVAL_TIMEOUT,
                            }),
                            Err(error) => {
                                tracing::warn!(%error, %session_id, "could not relay a sandbox approval");
                                let _ = reply.send(ApprovalDecision::Deny);
                            }
                        }
                    }
                    _ = approval_poll.tick(), if !relayed.is_empty() => {
                        settle_relayed_approvals(&poll_store, &mut relayed, yielding);
                    }
                    _ = heartbeat.tick() => {
                        if let Err(error) = poll_store.heartbeat(session_id, &poll_host_id) {
                            tracing::warn!(%error, %session_id, "session heartbeat failed");
                        }
                        if !yielding
                            && let Some(release) = release.as_ref()
                            && poll_store.release_requested(session_id, &poll_host_id).unwrap_or(false)
                        {
                            // Stop taking deliveries but keep the lease and
                            // keep settling: the host exits after its current
                            // turn, and the opener waits for the lease to end.
                            yielding = true;
                            release.notify_one();
                        }
                    }
                    _ = supervise.tick() => {
                        supervise_threads(&poll_store, tasks.as_ref(), &launcher, session_id).await;
                        ask_for_threads(&poll_store, &gate, &asking, session_id);
                    }
                    _ = inbox.tick(), if !yielding => {
                        match poll_store.next_delivery(session_id, &poll_host_id) {
                            Ok(Some(delivery)) => {
                                tracker.delivered(&delivery.message_id, delivery.payload.is_thread());
                                if wake_tx.send(delivery.wake()).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => tracing::warn!(%error, %session_id, "session inbox poll failed"),
                        }
                    }
                    event = events.recv(), if events_open => {
                        let event = match event {
                            Ok(event) => event,
                            // A lagged observer only misses outcomes; those
                            // deliveries stay unacknowledged and are offered
                            // again to the next host incarnation.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                events_open = false;
                                continue;
                            }
                        };
                        if event.session_id != session_id {
                            continue;
                        }
                        let Some(observed) = tracker.observe(&event) else {
                            continue;
                        };
                        settle(&poll_store, tasks.as_ref(), session_id, coordinator, observed).await;
                    }
                }
            }
        });
        Ok(Self {
            store,
            session_id,
            host_id,
            task,
        })
    }
}

impl Drop for CoordinationHost {
    fn drop(&mut self) {
        self.task.abort();
        self.store.mark_offline(self.session_id, &self.host_id);
    }
}

/// Hand answered relayed escalations back to the commands waiting on them.
/// Unanswered ones are denied when they time out, or when this host is
/// yielding the session, so the turn ends and the opener takes over.
fn settle_relayed_approvals(
    store: &CoordinationStore,
    relayed: &mut Vec<RelayedApproval>,
    yielding: bool,
) {
    let now = tokio::time::Instant::now();
    let mut waiting = Vec::with_capacity(relayed.len());
    for approval in relayed.drain(..) {
        let answer = match store.approval_answer(&approval.approval_id, &approval.request_digest) {
            Ok(Some(decision)) => Some(decision),
            Ok(None) if yielding || now >= approval.deadline => Some(ApprovalDecision::Deny),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "could not read a relayed sandbox approval");
                None
            }
        };
        match answer {
            Some(decision) => {
                store.drop_approval(&approval.approval_id);
                let _ = approval.reply.send(decision);
            }
            None => waiting.push(approval),
        }
    }
    *relayed = waiting;
}

/// Ask the person about this coordinator's threads' pending escalations
/// through its own approval prompt, one request at a time per approval.
fn ask_for_threads(
    store: &CoordinationStore,
    gate: &Arc<ApprovalGate>,
    asking: &Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    coordinator: SessionId,
) {
    let pending = match store.pending_approvals(coordinator) {
        Ok(pending) => pending,
        Err(error) => {
            tracing::warn!(%error, %coordinator, "could not read thread approvals");
            return;
        }
    };
    for approval in pending {
        if !asking
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(approval.approval_id.clone())
        {
            continue;
        }
        let store = store.clone();
        let gate = gate.clone();
        let asking = asking.clone();
        tokio::spawn(async move {
            let thread = approval.thread_id.uuid().simple().to_string();
            let request = ApprovalRequest {
                reason: format!(
                    "thread {}: {}",
                    &thread[..thread.len().min(8)],
                    approval.request.reason
                ),
                ..approval.request
            };
            // A host with no prompt surface denies, as for its own commands.
            let decision = gate.ask(request).await.unwrap_or(ApprovalDecision::Deny);
            if let Err(error) = store.answer_approval(&approval.approval_id, decision) {
                tracing::warn!(%error, "could not answer a thread approval");
            }
            asking
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&approval.approval_id);
        });
    }
}

/// Keep this coordinator's threads hosted: start a host for every thread with
/// work waiting, and restart one that vanished while its assignment was still
/// running. A thread waiting on an answer, or done, may stay stopped.
async fn supervise_threads(
    store: &CoordinationStore,
    tasks: &dyn SessionTaskRegistry,
    launcher: &ThreadLauncher,
    session_id: SessionId,
) {
    // A thread host that died outright left its commands running; stop them
    // before a successor starts on the same workspace.
    let stopped = crate::exec::group_ledger::reap_dead_hosts(&store.sessions_dir);
    if stopped > 0 {
        tracing::info!(stopped, "stopped commands left by a dead thread host");
    }
    let waiting = store.threads_needing_host(session_id);
    let hostless = store.hostless_threads(session_id);
    let (waiting, hostless) = match (waiting, hostless) {
        (Ok(waiting), Ok(hostless)) => (waiting, hostless),
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, %session_id, "thread supervision failed");
            return;
        }
    };
    for thread in waiting {
        if let Err(error) = ensure_thread_host(store, launcher, &thread) {
            tracing::warn!(%error, thread = %thread.thread_id, "could not start thread host");
        }
    }
    if hostless.is_empty() {
        return;
    }
    let assignments = match tasks
        .list(
            session_id,
            Some(&SessionTaskFilter {
                kind: Some(TASK_KIND_ASSIGNMENT.to_string()),
                state: None,
            }),
        )
        .await
    {
        Ok(assignments) => assignments,
        Err(error) => {
            tracing::warn!(%error, %session_id, "thread supervision failed");
            return;
        }
    };
    for thread in hostless {
        let running = assignments
            .iter()
            .filter(|task| task.links.child_session_id == Some(thread.thread_id))
            .max_by_key(|task| task.created_at)
            .is_some_and(|task| {
                matches!(
                    task.state,
                    everruns_core::SessionTaskState::Running
                        | everruns_core::SessionTaskState::Queued
                ) && task.input_request.is_none()
            });
        if !running {
            continue;
        }
        let result = store
            .enqueue(
                thread.thread_id,
                Some(session_id),
                &InboxPayload::Thread {
                    text: RESUME_NOTE.to_string(),
                },
            )
            .and_then(|_| ensure_thread_host(store, launcher, &thread));
        if let Err(error) = result {
            tracing::warn!(%error, thread = %thread.thread_id, "could not restart thread host");
        }
    }
}

/// How the turn woken by a delivery went.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TurnOutcome {
    Started,
    Completed,
    /// Cancelled by the person at the keyboard, who is now steering.
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Observed {
    message_id: String,
    thread: bool,
    outcome: TurnOutcome,
}

/// Correlates deliveries this host woke with the turns that processed them.
///
/// The wake input carries the inbox message id in host-only metadata (users
/// and editors cannot set it), the input message id ties it to a turn, and the
/// turn's events say how it went. Only message ids this host delivered are
/// honoured.
#[derive(Default)]
struct DeliveryTracker {
    /// Delivered message ids, and whether each is a thread message.
    in_flight: std::collections::HashMap<String, bool>,
    inputs: std::collections::HashMap<everruns_contracts::typed_id::MessageId, String>,
    turns: std::collections::HashMap<
        everruns_contracts::typed_id::TurnId,
        everruns_contracts::typed_id::MessageId,
    >,
}

impl DeliveryTracker {
    fn delivered(&mut self, message_id: &str, thread: bool) {
        self.in_flight.insert(message_id.to_string(), thread);
    }

    fn observe(&mut self, event: &everruns_core::Event) -> Option<Observed> {
        use everruns_core::EventData;
        let (turn_id, outcome) = match &event.data {
            EventData::InputMessage(data) => {
                let delivery = data
                    .message
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get(DELIVERY_METADATA_KEY))
                    .and_then(Value::as_str)
                    .filter(|id| self.in_flight.contains_key(*id))?;
                self.inputs.insert(data.message.id, delivery.to_string());
                return None;
            }
            EventData::TurnStarted(data) => {
                let message_id = self.inputs.get(&data.input_message_id)?;
                self.turns.insert(data.turn_id, data.input_message_id);
                return Some(Observed {
                    message_id: message_id.clone(),
                    thread: self.in_flight.get(message_id).copied().unwrap_or(false),
                    outcome: TurnOutcome::Started,
                });
            }
            EventData::TurnCompleted(data) => (data.turn_id, TurnOutcome::Completed),
            EventData::TurnCancelled(data) => (data.turn_id, TurnOutcome::Cancelled),
            EventData::TurnFailed(data) => (data.turn_id, TurnOutcome::Failed(data.error.clone())),
            _ => return None,
        };
        let input = self.turns.remove(&turn_id)?;
        let message_id = self.inputs.remove(&input)?;
        let thread = self.in_flight.remove(&message_id).unwrap_or(false);
        Some(Observed {
            message_id,
            thread,
            outcome,
        })
    }
}

/// Settle one observed delivery: the thread's assignment (when this session is
/// a thread and the turn came from its coordinator) and the inbox row.
async fn settle(
    store: &CoordinationStore,
    tasks: &dyn SessionTaskRegistry,
    session_id: SessionId,
    coordinator: Option<SessionId>,
    observed: Observed,
) {
    if observed.thread
        && let Some(coordinator) = coordinator
        && !(observed.outcome == TurnOutcome::Completed
            && waiting_on_own_work(tasks, session_id).await)
    {
        let turn = match &observed.outcome {
            TurnOutcome::Started => ThreadTurn::Started,
            TurnOutcome::Completed => ThreadTurn::Completed,
            TurnOutcome::Cancelled => ThreadTurn::Cancelled,
            TurnOutcome::Failed(_) => ThreadTurn::Failed,
        };
        if let Err(error) = settle_thread_turn(tasks, coordinator, session_id, turn).await {
            tracing::warn!(%error, %session_id, "thread turn settle failed");
        }
    }
    let result = match observed.outcome {
        TurnOutcome::Started => return,
        // A failed thread turn already failed its assignment, which wakes the
        // coordinator; replaying the brief would race its decision.
        TurnOutcome::Failed(_) if !observed.thread => store.nack(&observed.message_id),
        _ => store.ack(&observed.message_id),
    };
    if let Err(error) = result {
        tracing::warn!(%error, %session_id, "inbox settle failed");
    }
}

async fn waiting_on_own_work(tasks: &dyn SessionTaskRegistry, session_id: SessionId) -> bool {
    tasks
        .list(session_id, None)
        .await
        .is_ok_and(|tasks| crate::session_state::task_completion::has_pending_execution(&tasks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns::local::{LocalSessionTaskRegistry, SqliteDb};
    use everruns_core::session_task::{CreateSessionTask, TaskLinks, TaskWakePolicy};
    use everruns_core::{SessionTaskState, SessionTaskUpdate};
    use std::sync::Mutex;

    fn test_store() -> (CoordinationStore, Arc<LocalSessionTaskRegistry>) {
        let store = CoordinationStore::ephemeral().unwrap();
        let tasks =
            Arc::new(LocalSessionTaskRegistry::new(SqliteDb::open_in_memory().unwrap()).unwrap());
        (store, tasks)
    }

    fn recording_launcher() -> (ThreadLauncher, Arc<Mutex<Vec<SessionId>>>) {
        let started = Arc::new(Mutex::new(Vec::new()));
        let record = started.clone();
        let launcher: ThreadLauncher = Arc::new(move |thread: &ThreadRecord| {
            record.lock().unwrap().push(thread.thread_id);
            Ok(())
        });
        (launcher, started)
    }

    async fn assignment(
        tasks: &dyn SessionTaskRegistry,
        coordinator: SessionId,
        thread: SessionId,
    ) -> SessionTask {
        tasks
            .create(CreateSessionTask {
                session_id: coordinator,
                id: None,
                kind: TASK_KIND_ASSIGNMENT.to_string(),
                display_name: "Fix the bug".to_string(),
                spec: serde_json::json!({ "title": "Fix the bug", "brief": "b", "worker": "self" }),
                state: SessionTaskState::Running,
                links: TaskLinks {
                    child_session_id: Some(thread),
                    ..Default::default()
                },
                wake_policy: TaskWakePolicy::OnActivity,
            })
            .await
            .unwrap()
    }

    fn routing(store: &CoordinationStore, launcher: ThreadLauncher) -> ThreadRouting {
        ThreadRouting::new(
            store.clone(),
            launcher,
            PathBuf::from("/repo"),
            vec!["--profile".into(), "coordinator".into()],
            PathBuf::from("/nonexistent-sessions"),
        )
    }

    fn make_available_now(store: &CoordinationStore) {
        let inboxes = store.coordination_dir().join("inbox");
        let Ok(entries) = std::fs::read_dir(&inboxes) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(files) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if let Ok(Some(mut message)) = read_json::<InboxFile>(&path) {
                    message.available_at_ms = 0;
                    write_json(&path, &message).unwrap();
                }
            }
        }
    }

    #[test]
    fn launch_args_keep_only_inherited_flags() {
        let args = [
            "yolop",
            "--profile",
            "coordinator",
            "--acp",
            "--provider=anthropic",
            "--session",
            "session_x",
            "--data-dir",
            "/d",
            "-p",
            "hi",
        ]
        .map(String::from);
        assert_eq!(
            inherited_launch_args(args),
            [
                "--profile",
                "coordinator",
                "--provider=anthropic",
                "--data-dir",
                "/d"
            ]
            .map(String::from)
        );
    }

    #[test]
    fn launch_args_inherit_sandbox_containment() {
        // Coordinator started with `--sandbox` must propagate the WorkspaceWrite
        // override so the child does not fall back to DangerFullAccess.
        let args = ["yolop", "--sandbox", "--profile", "coordinator", "run"].map(String::from);
        assert_eq!(
            inherited_launch_args(args),
            ["--sandbox", "--profile", "coordinator"].map(String::from)
        );
    }

    #[test]
    fn launch_args_without_sandbox_stays_without_it() {
        let args = ["yolop", "--profile", "coordinator", "run"].map(String::from);
        assert_eq!(
            inherited_launch_args(args),
            ["--profile", "coordinator"].map(String::from)
        );
    }

    #[test]
    fn launch_args_sandbox_does_not_swallow_following_value() {
        // `--sandbox` takes no value; the next flag's value must survive.
        let args = [
            "yolop",
            "--sandbox",
            "--profile",
            "coordinator",
            "--provider=anthropic",
        ]
        .map(String::from);
        assert_eq!(
            inherited_launch_args(args),
            [
                "--sandbox",
                "--profile",
                "coordinator",
                "--provider=anthropic"
            ]
            .map(String::from)
        );
    }

    #[tokio::test]
    async fn a_session_becomes_a_thread_only_once_its_coordinator_assigns_it() {
        let (store, tasks) = test_store();
        let (launcher, started) = recording_launcher();
        let routing = routing(&store, launcher);
        let coordinator = SessionId::new();
        let thread = SessionId::new();

        // An in-process subagent child has no assignment: it is not a thread.
        assert!(
            routing
                .thread(thread, Some(coordinator), Some(tasks.as_ref()))
                .await
                .unwrap()
                .is_none()
        );

        assignment(tasks.as_ref(), coordinator, thread).await;
        let record = routing
            .thread(thread, Some(coordinator), Some(tasks.as_ref()))
            .await
            .unwrap()
            .expect("an assigned child is a thread");
        assert_eq!(record.coordinator, coordinator);
        assert_eq!(record.launch, ["--profile", "coordinator"]);
        // Recorded durably: a restarted coordinator with no session catalog
        // still routes to it.
        assert_eq!(
            routing.thread(thread, None, None).await.unwrap(),
            Some(record.clone())
        );

        routing.deliver(&record, "New assignment").unwrap();
        routing
            .deliver(&record, "Message from the coordinator")
            .unwrap();
        assert_eq!(
            *started.lock().unwrap(),
            vec![thread],
            "one host start per backoff window"
        );
    }

    #[tokio::test]
    async fn a_coordinator_restarts_threads_that_have_work_and_no_host() {
        let (store, tasks) = test_store();
        let (launcher, started) = recording_launcher();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        let task = assignment(tasks.as_ref(), coordinator, thread).await;
        // A finished assignment: the thread may stay stopped.
        finish(
            tasks.as_ref(),
            coordinator,
            &task.id,
            SessionTaskState::Succeeded,
        )
        .await;
        let record = routing(&store, launcher.clone())
            .thread(thread, Some(coordinator), Some(tasks.as_ref()))
            .await
            .unwrap()
            .unwrap();

        // Nothing waiting: nothing to start.
        supervise_threads(&store, tasks.as_ref(), &launcher, coordinator).await;
        assert!(started.lock().unwrap().is_empty());

        store
            .enqueue(
                thread,
                Some(coordinator),
                &InboxPayload::Thread { text: "go".into() },
            )
            .unwrap();
        supervise_threads(&store, tasks.as_ref(), &launcher, coordinator).await;
        assert_eq!(*started.lock().unwrap(), vec![thread]);

        // A live host needs no start, even after the backoff.
        store
            .register_host(thread, "host_t", "p", &record.workspace_root, true)
            .unwrap();
        let thread_file = store.thread_path(&thread.to_string());
        let mut record = read_json::<ThreadFile>(&thread_file).unwrap().unwrap();
        record.spawned_at_ms = 0;
        write_json(&thread_file, &record).unwrap();
        supervise_threads(&store, tasks.as_ref(), &launcher, coordinator).await;
        assert_eq!(started.lock().unwrap().len(), 1);

        // Another coordinator's supervision never touches this thread.
        store.mark_offline(thread, "host_t");
        supervise_threads(&store, tasks.as_ref(), &launcher, SessionId::new()).await;
        assert_eq!(started.lock().unwrap().len(), 1);
    }

    async fn finish(
        tasks: &dyn SessionTaskRegistry,
        coordinator: SessionId,
        task_id: &str,
        state: SessionTaskState,
    ) {
        tasks
            .update(
                coordinator,
                task_id,
                SessionTaskUpdate {
                    state: Some(state),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_thread_that_died_mid_assignment_is_restarted_with_a_note() {
        let (store, tasks) = test_store();
        let (launcher, started) = recording_launcher();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        let waiting = SessionId::new();
        assignment(tasks.as_ref(), coordinator, thread).await;
        let question = assignment(tasks.as_ref(), coordinator, waiting).await;
        tasks
            .update(
                coordinator,
                &question.id,
                SessionTaskUpdate {
                    state: Some(SessionTaskState::AwaitingInput),
                    input_request: Some(everruns_core::session_task::TaskInputRequest {
                        id: "q1".into(),
                        prompt: "Which branch?".into(),
                        expected: None,
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let routing = routing(&store, launcher.clone());
        for id in [thread, waiting] {
            routing
                .thread(id, Some(coordinator), Some(tasks.as_ref()))
                .await
                .unwrap()
                .unwrap();
        }

        supervise_threads(&store, tasks.as_ref(), &launcher, coordinator).await;
        // Only the thread that was working comes back; the one waiting on an
        // answer stays stopped until the coordinator messages it.
        assert_eq!(*started.lock().unwrap(), vec![thread]);
        let note = store.next_delivery(thread, "host_new").unwrap().unwrap();
        assert_eq!(
            note.payload,
            InboxPayload::Thread {
                text: RESUME_NOTE.to_string()
            }
        );

        // Once is enough while the restart is under way.
        supervise_threads(&store, tasks.as_ref(), &launcher, coordinator).await;
        assert_eq!(started.lock().unwrap().len(), 1);
    }

    #[test]
    fn only_a_live_headless_host_is_asked_to_yield() {
        let (store, _tasks) = test_store();
        let served = SessionId::new();
        let interactive = SessionId::new();
        store
            .register_host(served, "host_s", "p", Path::new("/repo"), true)
            .unwrap();
        store
            .register_host(interactive, "host_i", "p", Path::new("/repo"), false)
            .unwrap();
        assert!(!store.request_release(interactive).unwrap());
        assert!(!store.request_release(SessionId::new()).unwrap());
        assert!(!store.release_requested(served, "host_s").unwrap());
        assert!(store.request_release(served).unwrap());
        assert!(store.release_requested(served, "host_s").unwrap());
        // The next incarnation starts with a clean slate.
        store
            .register_host(served, "host_s2", "p", Path::new("/repo"), true)
            .unwrap();
        assert!(!store.release_requested(served, "host_s2").unwrap());
    }

    #[tokio::test]
    async fn opening_a_served_thread_waits_for_its_host_and_holds_off_a_restart() {
        let (store, _tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        store
            .record_thread(&ThreadRecord {
                thread_id: thread,
                coordinator,
                workspace_root: PathBuf::from("/repo"),
                launch: Vec::new(),
            })
            .unwrap();
        store
            .enqueue(
                thread,
                Some(coordinator),
                &InboxPayload::Thread { text: "go".into() },
            )
            .unwrap();
        store
            .register_host(thread, "host_t", "p", Path::new("/repo"), true)
            .unwrap();

        // The host finishes its turn and exits shortly after being asked.
        let exiting = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            exiting.mark_offline(thread, "host_t");
        });
        assert!(
            store
                .take_over(thread, Duration::from_secs(10))
                .await
                .unwrap()
        );
        // Work is pending and nothing holds the thread, yet the coordinator
        // leaves it to the person opening it.
        assert!(store.threads_needing_host(coordinator).unwrap().is_empty());
    }

    fn escalation(command: &str) -> ApprovalRequest {
        ApprovalRequest {
            command: command.into(),
            reason: "needs the network".into(),
            scope: ApprovalScope::FullAccess,
        }
    }

    #[test]
    fn approval_text_is_rejected_instead_of_shortened() {
        let (store, _tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        store
            .register_host(thread, "host_t", "p", Path::new("/repo"), true)
            .unwrap();
        for request in [
            escalation(&"x".repeat(MAX_MESSAGE_BYTES + 1)),
            ApprovalRequest {
                reason: "x".repeat(MAX_MESSAGE_BYTES + 1),
                ..escalation("dummy")
            },
        ] {
            assert!(
                store
                    .request_approval(thread, coordinator, "host_t", &request)
                    .is_err(),
                "oversized approval text must be rejected"
            );
            assert!(store.pending_approvals(coordinator).unwrap().is_empty());
        }
    }

    #[test]
    fn approval_text_round_trips_at_the_byte_boundary() {
        let (store, _tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        store
            .register_host(thread, "host_t", "p", Path::new("/repo"), true)
            .unwrap();
        let mut request = escalation(&"é".repeat(MAX_MESSAGE_BYTES / 2));
        request.reason = "ordinary reason".into();
        store
            .request_approval(thread, coordinator, "host_t", &request)
            .unwrap();
        let pending = store.pending_approvals(coordinator).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].request.command, request.command);
        assert_eq!(pending[0].request.reason, request.reason);
        assert_eq!(pending[0].request.scope, request.scope);
    }

    async fn answer_for(
        store: &CoordinationStore,
        approval_id: &str,
        request: &ApprovalRequest,
    ) -> ApprovalDecision {
        for _ in 0..200 {
            if let Some(decision) = store
                .approval_answer(approval_id, &request.digest())
                .unwrap()
            {
                return decision;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("approval {approval_id} was never answered");
    }

    #[tokio::test]
    async fn ordinary_approval_relay_preserves_once_and_session_decisions() {
        for decision in [
            ApprovalDecision::ApproveOnce,
            ApprovalDecision::ApproveForSession,
        ] {
            let (store, _tasks) = test_store();
            let coordinator = SessionId::new();
            let thread = SessionId::new();
            store
                .register_host(thread, "host_t", "p", Path::new("/repo"), true)
                .unwrap();
            let request = escalation("dummy review text");
            let expected_digest = request.digest();
            let (thread_gate, mut relay) = ApprovalGate::channel();
            let waiting_gate = thread_gate.clone();
            let original = request.clone();
            let waiting = tokio::spawn(async move { waiting_gate.approve(original).await });
            let (relayed_request, reply) = relay.recv().await.unwrap();
            assert_eq!(relayed_request.digest(), expected_digest);
            let approval_id = store
                .request_approval(thread, coordinator, "host_t", &relayed_request)
                .unwrap();
            let (coordinator_gate, mut prompts) = ApprovalGate::channel();
            let asking = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
            ask_for_threads(&store, &coordinator_gate, &asking, coordinator);
            let (displayed, answer) = prompts.recv().await.unwrap();
            assert_eq!(displayed.command, request.command);
            assert!(displayed.reason.ends_with(&request.reason));
            assert_eq!(displayed.scope, request.scope);
            answer.send(decision).unwrap();
            assert_eq!(answer_for(&store, &approval_id, &request).await, decision);
            let mut relayed = vec![RelayedApproval {
                approval_id: approval_id.clone(),
                request_digest: expected_digest,
                reply,
                deadline: tokio::time::Instant::now() + APPROVAL_TIMEOUT,
            }];
            settle_relayed_approvals(&store, &mut relayed, false);
            assert!(waiting.await.unwrap());
            assert!(relayed.is_empty());
            assert!(store.pending_approvals(coordinator).unwrap().is_empty());
            let again = thread_gate.approve(escalation("next dummy"));
            let respond = async {
                if decision == ApprovalDecision::ApproveOnce {
                    let (_, reply) = relay.recv().await.unwrap();
                    reply.send(ApprovalDecision::Deny).unwrap();
                }
            };
            let (approved, ()) = tokio::join!(again, respond);
            assert_eq!(approved, decision == ApprovalDecision::ApproveForSession);
            assert!(relay.try_recv().is_err());
        }
    }

    #[test]
    fn legacy_approval_records_fail_closed_before_and_after_a_decision() {
        let (store, _tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        store
            .register_host(thread, "host_t", "p", Path::new("/repo"), true)
            .unwrap();
        let request = escalation("legacy dummy");
        for decision in [
            None,
            Some(ApprovalDecision::ApproveOnce),
            Some(ApprovalDecision::ApproveForSession),
        ] {
            let approval_id = format!("legacy-{}", unique_suffix());
            let approval = ApprovalFile {
                approval_id: approval_id.clone(),
                thread_id: thread.to_string(),
                coordinator: coordinator.to_string(),
                host_id: "host_t".to_string(),
                command: request.command.clone(),
                reason: request.reason.clone(),
                scope: request.scope.as_str().to_string(),
                decision: decision.map(|choice| choice.as_str().to_string()),
                request_digest: None,
                created_at_ms: now_ms(),
            };
            write_json(&store.approval_path(&approval_id), &approval).unwrap();
            assert!(store.pending_approvals(coordinator).unwrap().is_empty());
            assert_eq!(
                store
                    .approval_answer(&approval_id, &request.digest())
                    .unwrap(),
                Some(ApprovalDecision::Deny)
            );
        }
    }

    #[test]
    fn legacy_database_is_imported_once_then_retired() {
        let dir = tempfile::TempDir::new().unwrap();
        let legacy_dir = dir.path().join("everruns-local");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        let now = now_ms();
        {
            let conn = rusqlite::Connection::open(legacy_dir.join("local.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE yolop_session_presence (
                    session_id TEXT PRIMARY KEY,
                    host_id TEXT NOT NULL,
                    project_id TEXT NOT NULL DEFAULT '',
                    workspace_root TEXT NOT NULL,
                    heartbeat_at_ms INTEGER NOT NULL,
                    headless INTEGER NOT NULL DEFAULT 0,
                    release_requested_at_ms INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE yolop_session_inbox (
                    message_id TEXT PRIMARY KEY,
                    target_session_id TEXT NOT NULL,
                    source_session_id TEXT,
                    payload_json TEXT NOT NULL,
                    delivered_host_id TEXT,
                    created_at_ms INTEGER NOT NULL,
                    acked_at_ms INTEGER,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    available_at_ms INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE yolop_threads (
                    thread_id TEXT PRIMARY KEY,
                    coordinator_session_id TEXT NOT NULL,
                    workspace_root TEXT NOT NULL,
                    launch_json TEXT NOT NULL DEFAULT '[]',
                    created_at_ms INTEGER NOT NULL,
                    spawned_at_ms INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE yolop_thread_approvals (
                    approval_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    thread_id TEXT NOT NULL,
                    coordinator_session_id TEXT NOT NULL,
                    host_id TEXT NOT NULL,
                    command TEXT NOT NULL,
                    reason TEXT NOT NULL,
                    scope TEXT NOT NULL,
                    decision TEXT,
                    created_at_ms INTEGER NOT NULL
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO yolop_session_presence
                 VALUES (?1, 'host_m', 'p', '/repo', ?2, 1, 0)",
                rusqlite::params![coordinator.to_string(), now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO yolop_threads VALUES (?1, ?2, '/repo', '[]', ?3, 0)",
                rusqlite::params![thread.to_string(), coordinator.to_string(), now],
            )
            .unwrap();
            let payload = serde_json::to_string(&InboxPayload::Message {
                from: None,
                text: "hi".into(),
            })
            .unwrap();
            conn.execute(
                "INSERT INTO yolop_session_inbox
                 VALUES ('cmsg_migrated', ?1, NULL, ?2, NULL, ?3, NULL, 0, 0)",
                rusqlite::params![thread.to_string(), payload, now],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO yolop_thread_approvals
                 (thread_id, coordinator_session_id, host_id, command, reason, scope, decision, created_at_ms)
                 VALUES (?1, ?2, 'host_m', 'legacy', 'legacy dummy message', 'read', NULL, ?3)",
                rusqlite::params![thread.to_string(), coordinator.to_string(), now],
            )
            .unwrap();
        }
        let store = CoordinationStore::open(dir.path()).unwrap();
        assert!(store.is_live(coordinator).unwrap());
        assert_eq!(
            store.thread(thread).unwrap().unwrap().coordinator,
            coordinator
        );
        let delivery = store.next_delivery(thread, "host_new").unwrap().unwrap();
        assert_eq!(delivery.message_id, "cmsg_migrated");
        assert!(store.pending_approvals(coordinator).unwrap().is_empty());
        assert!(!legacy_dir.join("local.db").exists());
        let mut migrated = false;
        for entry in std::fs::read_dir(&legacy_dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            migrated |= name.starts_with("local.db.migrated-");
        }
        assert!(migrated, "legacy database is kept as a backup");
        assert!(dir.path().join("coordination/migrated-v1").exists());
        let reopened = CoordinationStore::open(dir.path()).unwrap();
        assert_eq!(reopened.all_threads().unwrap().len(), 1);
    }

    #[test]
    fn unrecognized_approval_scope_is_denied_without_repeated_presentation() {
        let (store, _tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        store
            .register_host(thread, "host_t", "p", Path::new("/repo"), true)
            .unwrap();
        let approval_id = format!("unscoped-{}", unique_suffix());
        let approval = ApprovalFile {
            approval_id: approval_id.clone(),
            thread_id: thread.to_string(),
            coordinator: coordinator.to_string(),
            host_id: "host_t".to_string(),
            command: "dummy".to_string(),
            reason: "review".to_string(),
            scope: "unknown_scope".to_string(),
            decision: None,
            request_digest: None,
            created_at_ms: now_ms(),
        };
        write_json(&store.approval_path(&approval_id), &approval).unwrap();
        assert!(store.pending_approvals(coordinator).unwrap().is_empty());
        let decision = read_json::<ApprovalFile>(&store.approval_path(&approval_id))
            .unwrap()
            .unwrap()
            .decision;
        assert_eq!(decision.as_deref(), Some("deny"));
        assert!(store.pending_approvals(coordinator).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_headless_threads_escalation_is_asked_by_its_coordinator() {
        let (store, _tasks) = test_store();
        let coordinator = SessionId::new();
        let other = SessionId::new();
        let thread = SessionId::new();
        store
            .register_host(thread, "host_t", "p", Path::new("/repo"), true)
            .unwrap();
        let approval_id = store
            .request_approval(thread, coordinator, "host_t", &escalation("cargo fetch"))
            .unwrap();
        // Only the thread's own coordinator sees it.
        assert!(store.pending_approvals(other).unwrap().is_empty());

        let (gate, mut prompts) = ApprovalGate::channel();
        let asking = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        ask_for_threads(&store, &gate, &asking, coordinator);
        // A second sweep while the person is deciding does not ask again.
        ask_for_threads(&store, &gate, &asking, coordinator);
        let (prompt, reply) = prompts.recv().await.unwrap();
        assert_eq!(prompt.command, "cargo fetch");
        assert!(
            prompt.reason.ends_with("needs the network"),
            "{}",
            prompt.reason
        );
        assert!(prompt.reason.starts_with("thread "), "{}", prompt.reason);
        reply.send(ApprovalDecision::ApproveForSession).unwrap();
        assert_eq!(
            answer_for(&store, &approval_id, &escalation("cargo fetch")).await,
            ApprovalDecision::ApproveForSession
        );
        assert!(prompts.try_recv().is_err());
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let mut relayed = vec![RelayedApproval {
            approval_id: approval_id.clone(),
            request_digest: escalation("cargo fetch").digest(),
            reply: reply_tx,
            deadline: tokio::time::Instant::now() + APPROVAL_TIMEOUT,
        }];
        settle_relayed_approvals(&store, &mut relayed, false);
        assert!(relayed.is_empty());
        assert_eq!(reply_rx.await.unwrap(), ApprovalDecision::ApproveForSession);
        assert_eq!(
            store
                .approval_answer(&approval_id, &escalation("cargo fetch").digest())
                .unwrap(),
            None
        );

        // A coordinator with no prompt surface denies.
        let denied = store
            .request_approval(thread, coordinator, "host_t", &escalation("curl"))
            .unwrap();
        ask_for_threads(&store, &ApprovalGate::deny(), &asking, coordinator);
        assert_eq!(
            answer_for(&store, &denied, &escalation("curl")).await,
            ApprovalDecision::Deny
        );
    }

    #[tokio::test]
    async fn an_escalation_nobody_answers_is_denied_when_the_thread_is_opened() {
        let (store, _tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        store
            .register_host(thread, "host_t", "p", Path::new("/repo"), true)
            .unwrap();
        let approval_id = store
            .request_approval(thread, coordinator, "host_t", &escalation("make"))
            .unwrap();
        let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
        let mut relayed = vec![RelayedApproval {
            approval_id: approval_id.clone(),
            request_digest: escalation("make").digest(),
            reply: reply_tx,
            deadline: tokio::time::Instant::now() + APPROVAL_TIMEOUT,
        }];
        settle_relayed_approvals(&store, &mut relayed, false);
        assert_eq!(relayed.len(), 1, "still waiting on the coordinator");
        assert!(reply_rx.try_recv().is_err());
        settle_relayed_approvals(&store, &mut relayed, true);
        assert!(relayed.is_empty());
        assert_eq!(reply_rx.await.unwrap(), ApprovalDecision::Deny);
        // A dead host's leftover request is never put to the person.
        store
            .request_approval(thread, coordinator, "host_t", &escalation("make"))
            .unwrap();
        store.mark_offline(thread, "host_t");
        assert!(store.pending_approvals(coordinator).unwrap().is_empty());
    }

    #[test]
    fn an_unacknowledged_delivery_is_offered_to_the_next_host() {
        let (store, _tasks) = test_store();
        let session = SessionId::new();
        let message = InboxPayload::Message {
            from: None,
            text: "hello".into(),
        };
        store.enqueue(session, None, &message).unwrap();

        let first = store.next_delivery(session, "host_a").unwrap().unwrap();
        assert_eq!(first.payload, message);
        assert!(store.next_delivery(session, "host_a").unwrap().is_none());
        // The host died before its turn finished: a restarted session gets it.
        let again = store.next_delivery(session, "host_b").unwrap().unwrap();
        assert_eq!(again.message_id, first.message_id);
        store.ack(&again.message_id).unwrap();
        assert!(store.next_delivery(session, "host_c").unwrap().is_none());
        // Other sessions never see it.
        assert!(
            store
                .next_delivery(SessionId::new(), "host_d")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_failing_message_is_retried_then_dropped() {
        let (store, _tasks) = test_store();
        let session = SessionId::new();
        store
            .enqueue(
                session,
                None,
                &InboxPayload::Message {
                    from: None,
                    text: "x".into(),
                },
            )
            .unwrap();
        for attempt in 0..MAX_DELIVERY_ATTEMPTS {
            make_available_now(&store);
            let delivery = store
                .next_delivery(session, &format!("host_{attempt}"))
                .unwrap()
                .expect("offered again after a failure");
            store.nack(&delivery.message_id).unwrap();
            assert!(
                store.next_delivery(session, "host_wait").unwrap().is_none(),
                "a retry waits out its backoff"
            );
        }
        make_available_now(&store);
        assert!(
            store
                .next_delivery(session, "host_final")
                .unwrap()
                .is_none()
        );
    }

    fn input_event(
        session_id: SessionId,
        delivery: Option<&str>,
    ) -> (
        everruns_core::Event,
        everruns_contracts::typed_id::MessageId,
    ) {
        let mut message = everruns_core::RuntimeMessage::user("[automatic] work");
        if let Some(delivery) = delivery {
            message.metadata = Some(std::collections::HashMap::from([(
                DELIVERY_METADATA_KEY.to_string(),
                Value::String(delivery.to_string()),
            )]));
        }
        let id = message.id;
        (
            everruns_core::Event::new(
                session_id,
                everruns_core::EventContext::default(),
                everruns_core::events::InputMessageData::new(message),
            ),
            id,
        )
    }

    fn turn_started(
        session_id: SessionId,
        input: everruns_contracts::typed_id::MessageId,
    ) -> (everruns_core::Event, everruns_contracts::typed_id::TurnId) {
        let turn_id = everruns_contracts::typed_id::TurnId::new();
        (
            everruns_core::Event::new(
                session_id,
                everruns_core::EventContext::default(),
                everruns_core::TurnStartedData {
                    turn_id,
                    input_message_id: input,
                    origin: None,
                    input_content: None,
                    agent_id: None,
                    agent_name: None,
                    agent_description: None,
                },
            ),
            turn_id,
        )
    }

    fn turn_failed(
        session_id: SessionId,
        turn_id: everruns_contracts::typed_id::TurnId,
    ) -> everruns_core::Event {
        everruns_core::Event::new(
            session_id,
            everruns_core::EventContext::default(),
            everruns_core::TurnFailedData {
                turn_id,
                error: "credit balance too low".into(),
                error_code: None,
                error_fields: None,
                error_disclosure: None,
            },
        )
    }

    #[test]
    fn tracker_settles_only_deliveries_this_host_made() {
        let session = SessionId::new();
        let mut tracker = DeliveryTracker::default();
        tracker.delivered("cmsg_real", true);

        let (input, input_id) = input_event(session, Some("cmsg_real"));
        assert!(tracker.observe(&input).is_none());
        let (started, turn_id) = turn_started(session, input_id);
        assert_eq!(
            tracker.observe(&started),
            Some(Observed {
                message_id: "cmsg_real".into(),
                thread: true,
                outcome: TurnOutcome::Started,
            })
        );
        assert_eq!(
            tracker.observe(&turn_failed(session, turn_id)),
            Some(Observed {
                message_id: "cmsg_real".into(),
                thread: true,
                outcome: TurnOutcome::Failed("credit balance too low".into()),
            })
        );

        // An id this host never delivered (or an ordinary user turn) is ignored.
        let (forged, forged_id) = input_event(session, Some("cmsg_forged"));
        assert!(tracker.observe(&forged).is_none());
        let (started, turn_id) = turn_started(session, forged_id);
        assert!(tracker.observe(&started).is_none());
        assert!(tracker.observe(&turn_failed(session, turn_id)).is_none());
    }

    #[tokio::test]
    async fn a_thread_turn_that_ends_without_completing_wakes_its_coordinator() {
        let (store, tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        let task = assignment(tasks.as_ref(), coordinator, thread).await;
        // The thread's process: its registry reports to the coordinator's inbox.
        let observed = Arc::new(
            everruns_core::ObservingTaskRegistry::new(tasks.clone())
                .with_observer(Arc::new(CoordinatorInboxWaker::new(store.clone(), thread))),
        );
        let message_id = store
            .enqueue(
                thread,
                Some(coordinator),
                &InboxPayload::Thread {
                    text: "brief".into(),
                },
            )
            .unwrap();
        store.next_delivery(thread, "host_t").unwrap().unwrap();

        settle(
            &store,
            observed.as_ref(),
            thread,
            Some(coordinator),
            Observed {
                message_id: message_id.clone(),
                thread: true,
                outcome: TurnOutcome::Completed,
            },
        )
        .await;

        let task = tasks.get(coordinator, &task.id).await.unwrap().unwrap();
        assert_eq!(task.state, SessionTaskState::AwaitingInput);
        let update = store
            .next_delivery(coordinator, "host_c")
            .unwrap()
            .expect("the coordinator hears about it");
        match update.payload {
            InboxPayload::TaskUpdate { text } => assert!(text.contains("awaiting input")),
            other => panic!("unexpected payload {other:?}"),
        }
        // The brief itself was handled.
        assert!(store.next_delivery(thread, "host_t2").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_failed_thread_turn_fails_the_assignment_without_replaying_the_brief() {
        let (store, tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        let task = assignment(tasks.as_ref(), coordinator, thread).await;
        let message_id = store
            .enqueue(
                thread,
                Some(coordinator),
                &InboxPayload::Thread {
                    text: "brief".into(),
                },
            )
            .unwrap();
        store.next_delivery(thread, "host_t").unwrap().unwrap();

        settle(
            &store,
            tasks.as_ref(),
            thread,
            Some(coordinator),
            Observed {
                message_id,
                thread: true,
                outcome: TurnOutcome::Failed("stall".into()),
            },
        )
        .await;

        let task = tasks.get(coordinator, &task.id).await.unwrap().unwrap();
        assert_eq!(task.state, SessionTaskState::Failed);
        make_available_now(&store);
        assert!(store.next_delivery(thread, "host_t2").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_thread_waiting_on_its_own_background_work_is_not_flagged() {
        let (store, tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        let task = assignment(tasks.as_ref(), coordinator, thread).await;
        tasks
            .create(CreateSessionTask {
                session_id: thread,
                id: None,
                kind: "background_tool".to_string(),
                display_name: "soak run".to_string(),
                spec: serde_json::json!({}),
                state: SessionTaskState::Running,
                links: TaskLinks::default(),
                wake_policy: TaskWakePolicy::OnTerminal,
            })
            .await
            .unwrap();
        let message_id = store
            .enqueue(
                thread,
                Some(coordinator),
                &InboxPayload::Thread {
                    text: "brief".into(),
                },
            )
            .unwrap();
        store.next_delivery(thread, "host_t").unwrap().unwrap();

        settle(
            &store,
            tasks.as_ref(),
            thread,
            Some(coordinator),
            Observed {
                message_id,
                thread: true,
                outcome: TurnOutcome::Completed,
            },
        )
        .await;

        let task = tasks.get(coordinator, &task.id).await.unwrap().unwrap();
        assert_eq!(task.state, SessionTaskState::Running);
        assert!(task.input_request.is_none());
        assert!(store.next_delivery(thread, "host_t2").unwrap().is_none());
    }

    #[tokio::test]
    async fn the_coordinator_process_does_not_wake_itself() {
        let (store, tasks) = test_store();
        let coordinator = SessionId::new();
        let thread = SessionId::new();
        let task = assignment(tasks.as_ref(), coordinator, thread).await;
        let observed = everruns_core::ObservingTaskRegistry::new(tasks.clone()).with_observer(
            Arc::new(CoordinatorInboxWaker::new(store.clone(), coordinator)),
        );
        observed
            .update(
                coordinator,
                &task.id,
                SessionTaskUpdate {
                    state: Some(SessionTaskState::Canceled),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            store
                .next_delivery(coordinator, "host_c")
                .unwrap()
                .is_none()
        );
    }
}

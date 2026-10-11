//! Messaging between independently hosted Yolop sessions, and the
//! out-of-process thread host for the everruns `coordination` capability.
//!
//! Every session holds a presence lease and drains a durable inbox kept in the
//! local profile's SQLite database, so any session (or a shell outside one) can
//! write to any other: `yolop sessions send` is the generic form. The
//! coordination model itself is everruns': a coordinator starts threads, each
//! assignment is a session task it owns, and threads report through the
//! worker tools. What Yolop adds is where a thread runs. Upstream runs threads
//! inside the coordinator's process; here each thread is its own headless
//! `yolop --serve` process, so a thread survives its coordinator's restart and
//! a person can open it like any other session.
//!
//! Decision: the inbox is the only cross-process channel. A brief or relay for
//!   a thread, a thread's update for its coordinator, and a plain message all
//!   travel as inbox rows, acknowledged only when the turn they woke finishes.
//!   A host that dies first leaves them pending for its next incarnation.
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
//! Decision: a thread whose host vanished mid-assignment (crash, kill, reboot)
//!   is restarted with a note to check what survived, since its background
//!   processes may not have.

use crate::runtime::background_wake::{WakeMessage, WakeSender};
use everruns::local::SqliteDb;
use everruns_capabilities::capabilities::coordination::{
    THREAD_RESOLVED_DETAIL, ThreadTurn, settle_thread_turn,
};
use everruns_contracts::typed_id::SessionId;
use everruns_core::session_task::{SessionTaskFilter, TASK_KIND_ASSIGNMENT};
use everruns_core::{
    SessionTask, SessionTaskRegistry, TaskTransition, TaskTransitionObserver, wake_text_for,
};
use rusqlite::{OptionalExtension, params};
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
/// Launch flags a thread host inherits from the process that started it.
const INHERITED_FLAGS: [&str; 8] = [
    "--profile",
    "--provider",
    "--model",
    "--reasoning-effort",
    "--config-dir",
    "--data-dir",
    "--session-dir",
    "--env-context",
];

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

#[derive(Clone)]
pub(crate) struct CoordinationStore {
    db: SqliteDb,
    /// Where dead thread hosts' command ledgers live; `None` for in-memory
    /// stores, which supervise nothing on disk.
    sessions_dir: Option<PathBuf>,
}

impl CoordinationStore {
    pub(crate) fn new(db: SqliteDb) -> anyhow::Result<Self> {
        let store = Self {
            db,
            sessions_dir: None,
        };
        store.ensure_schema()?;
        Ok(store)
    }

    /// Supervise dead thread hosts' command ledgers under `sessions_dir`.
    pub(crate) fn with_sessions_dir(mut self, sessions_dir: &Path) -> Self {
        self.sessions_dir = Some(sessions_dir.to_path_buf());
        self
    }

    pub(crate) fn open(sessions_dir: &Path) -> anyhow::Result<Self> {
        let profile = everruns::local::LocalProfile::new(sessions_dir.join("everruns-local"));
        profile.ensure_dirs()?;
        Ok(Self::new(SqliteDb::open(profile.db_path())?)?.with_sessions_dir(sessions_dir))
    }

    fn ensure_schema(&self) -> anyhow::Result<()> {
        self.db.with_conn(|conn| {
            conn.execute_batch(
                // The worker-pool tables of the earlier yolop-only protocol;
                // everruns coordination threads replaced it before 1.0.
                "DROP TABLE IF EXISTS yolop_coordination_workers;
                 DROP TABLE IF EXISTS yolop_coordination_assignments;
                 DROP TABLE IF EXISTS yolop_coordination_inbox;
                 CREATE TABLE IF NOT EXISTS yolop_session_presence (
                    session_id TEXT PRIMARY KEY,
                    host_id TEXT NOT NULL,
                    project_id TEXT NOT NULL,
                    workspace_root TEXT NOT NULL,
                    heartbeat_at_ms INTEGER NOT NULL,
                    headless INTEGER NOT NULL DEFAULT 0,
                    release_requested_at_ms INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE IF NOT EXISTS yolop_session_inbox (
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
                 CREATE INDEX IF NOT EXISTS yolop_session_inbox_target
                    ON yolop_session_inbox(target_session_id, acked_at_ms, created_at_ms);
                 CREATE TABLE IF NOT EXISTS yolop_threads (
                    thread_id TEXT PRIMARY KEY,
                    coordinator_session_id TEXT NOT NULL,
                    workspace_root TEXT NOT NULL,
                    launch_json TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    spawned_at_ms INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE INDEX IF NOT EXISTS yolop_threads_coordinator
                    ON yolop_threads(coordinator_session_id);",
            )?;
            // Presence rows are leases that expire within seconds, so a table
            // from a build without the hand-over columns is rebuilt, not migrated.
            let has_release = conn
                .prepare("SELECT release_requested_at_ms FROM yolop_session_presence LIMIT 0")
                .is_ok();
            if !has_release {
                conn.execute_batch(
                    "DROP TABLE yolop_session_presence;
                     CREATE TABLE yolop_session_presence (
                        session_id TEXT PRIMARY KEY,
                        host_id TEXT NOT NULL,
                        project_id TEXT NOT NULL,
                        workspace_root TEXT NOT NULL,
                        heartbeat_at_ms INTEGER NOT NULL,
                        headless INTEGER NOT NULL DEFAULT 0,
                        release_requested_at_ms INTEGER NOT NULL DEFAULT 0
                     );",
                )?;
            }
            Ok(())
        })?;
        Ok(())
    }

    fn register_host(
        &self,
        session_id: SessionId,
        host_id: &str,
        project_id: &str,
        workspace_root: &Path,
        headless: bool,
    ) -> anyhow::Result<()> {
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO yolop_session_presence
                    (session_id, host_id, project_id, workspace_root, heartbeat_at_ms,
                     headless, release_requested_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
                 ON CONFLICT(session_id) DO UPDATE SET
                    host_id = excluded.host_id,
                    project_id = excluded.project_id,
                    workspace_root = excluded.workspace_root,
                    heartbeat_at_ms = excluded.heartbeat_at_ms,
                    headless = excluded.headless,
                    release_requested_at_ms = 0",
                params![
                    session_id.to_string(),
                    host_id,
                    project_id,
                    workspace_root.display().to_string(),
                    now_ms(),
                    headless,
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    fn heartbeat(&self, session_id: SessionId, host_id: &str) -> anyhow::Result<()> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_session_presence SET heartbeat_at_ms = ?3
                 WHERE session_id = ?1 AND host_id = ?2",
                params![session_id.to_string(), host_id, now_ms()],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    fn mark_offline(&self, session_id: SessionId, host_id: &str) {
        let _ = self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_session_presence SET heartbeat_at_ms = 0
                 WHERE session_id = ?1 AND host_id = ?2",
                params![session_id.to_string(), host_id],
            )?;
            Ok(())
        });
    }

    /// Whether some process currently hosts `session_id`.
    pub(crate) fn is_live(&self, session_id: SessionId) -> anyhow::Result<bool> {
        let live = self.db.with_conn(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM yolop_session_presence
                 WHERE session_id = ?1 AND heartbeat_at_ms >= ?2)",
                params![session_id.to_string(), now_ms() - PRESENCE_LEASE_MS],
                |row| row.get::<_, bool>(0),
            )
        })?;
        Ok(live)
    }

    /// Ask the headless host of `session_id` to hand the session over. False
    /// when no live headless host holds it (an interactive one never yields).
    pub(crate) fn request_release(&self, session_id: SessionId) -> anyhow::Result<bool> {
        let changed = self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_session_presence SET release_requested_at_ms = ?2
                 WHERE session_id = ?1 AND headless = 1 AND heartbeat_at_ms >= ?3",
                params![
                    session_id.to_string(),
                    now_ms(),
                    now_ms() - PRESENCE_LEASE_MS
                ],
            )
        })?;
        Ok(changed == 1)
    }

    /// Take a headless host's session over for a person opening it: ask the
    /// host to yield, wait (up to `timeout`) for its lease to end, and hold
    /// off the coordinator's restart of the thread meanwhile. False when the
    /// host was still live at the deadline.
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
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_threads SET spawned_at_ms = ?2 WHERE thread_id = ?1",
                params![thread_id.to_string(), now_ms()],
            )
        })?;
        Ok(())
    }

    fn release_requested(&self, session_id: SessionId, host_id: &str) -> anyhow::Result<bool> {
        let requested = self.db.with_conn(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM yolop_session_presence
                 WHERE session_id = ?1 AND host_id = ?2 AND release_requested_at_ms > 0)",
                params![session_id.to_string(), host_id],
                |row| row.get::<_, bool>(0),
            )
        })?;
        Ok(requested)
    }

    /// Every live session, newest heartbeat first.
    pub(crate) fn live_sessions(&self) -> anyhow::Result<Vec<LiveSession>> {
        let sessions = self.db.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT p.session_id, p.workspace_root, t.coordinator_session_id
                 FROM yolop_session_presence p
                 LEFT JOIN yolop_threads t ON t.thread_id = p.session_id
                 WHERE p.heartbeat_at_ms >= ?1
                 ORDER BY p.heartbeat_at_ms DESC",
            )?;
            let rows = statement.query_map(params![now_ms() - PRESENCE_LEASE_MS], |row| {
                Ok(LiveSession {
                    session_id: row.get(0)?,
                    workspace_root: row.get(1)?,
                    thread_of: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })?;
        Ok(sessions)
    }

    #[cfg(test)]
    pub(crate) fn live_session_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .live_sessions()?
            .into_iter()
            .map(|session| session.session_id)
            .collect())
    }

    /// Queue `payload` for `target`. It waits until a host of that session
    /// delivers it and the turn it wakes finishes.
    pub(crate) fn enqueue(
        &self,
        target: SessionId,
        source: Option<SessionId>,
        payload: &InboxPayload,
    ) -> anyhow::Result<String> {
        let message_id = new_message_id();
        let payload = serde_json::to_string(payload)?;
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO yolop_session_inbox
                    (message_id, target_session_id, source_session_id, payload_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    message_id,
                    target.to_string(),
                    source.map(|source| source.to_string()),
                    payload,
                    now_ms(),
                ],
            )?;
            Ok(())
        })?;
        Ok(message_id)
    }

    /// Claim the oldest unacknowledged message for this host incarnation.
    ///
    /// A message stays pending until the turn it woke finishes ([`Self::ack`]).
    /// A host that dies first leaves it claimed by a dead incarnation, so the
    /// next incarnation (a resumed session) receives it again.
    fn next_delivery(
        &self,
        session_id: SessionId,
        host_id: &str,
    ) -> anyhow::Result<Option<Delivery>> {
        let row = self.db.with_conn(|conn| {
            conn.query_row(
                "SELECT message_id, payload_json FROM yolop_session_inbox
                 WHERE target_session_id = ?1 AND acked_at_ms IS NULL
                   AND available_at_ms <= ?3
                   AND (delivered_host_id IS NULL OR delivered_host_id <> ?2)
                 ORDER BY created_at_ms ASC LIMIT 1",
                params![session_id.to_string(), host_id, now_ms()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
        })?;
        let Some((message_id, payload)) = row else {
            return Ok(None);
        };
        let claimed = self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_session_inbox
                 SET delivered_host_id = ?2, attempts = attempts + 1
                 WHERE message_id = ?1 AND acked_at_ms IS NULL
                   AND (delivered_host_id IS NULL OR delivered_host_id <> ?2)",
                params![message_id, host_id],
            )
        })?;
        if claimed != 1 {
            return Ok(None);
        }
        let payload = match serde_json::from_str(&payload) {
            Ok(payload) => payload,
            Err(error) => {
                // Unreadable rows (another build's format) are dropped rather
                // than offered forever.
                tracing::warn!(%error, %message_id, "dropping unreadable inbox message");
                self.ack(&message_id)?;
                return Ok(None);
            }
        };
        Ok(Some(Delivery {
            message_id,
            payload,
        }))
    }

    /// The turn a delivery woke finished; never offer it again.
    fn ack(&self, message_id: &str) -> anyhow::Result<()> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_session_inbox SET acked_at_ms = ?2
                 WHERE message_id = ?1 AND acked_at_ms IS NULL",
                params![message_id, now_ms()],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// The turn a delivery woke failed. Offer it again after a backoff until
    /// its attempts are spent, then give up on it.
    fn nack(&self, message_id: &str) -> anyhow::Result<()> {
        let attempts = self.db.with_conn(|conn| {
            conn.query_row(
                "SELECT attempts FROM yolop_session_inbox
                 WHERE message_id = ?1 AND acked_at_ms IS NULL",
                params![message_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
        })?;
        let Some(attempts) = attempts else {
            return Ok(());
        };
        if attempts >= MAX_DELIVERY_ATTEMPTS {
            return self.ack(message_id);
        }
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_session_inbox
                 SET delivered_host_id = NULL, available_at_ms = ?2
                 WHERE message_id = ?1 AND acked_at_ms IS NULL",
                params![
                    message_id,
                    now_ms() + DELIVERY_RETRY_BACKOFF_MS * attempts.max(1)
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Remember `thread` as a thread of its coordinator. Idempotent.
    pub(crate) fn record_thread(&self, thread: &ThreadRecord) -> anyhow::Result<()> {
        let launch = serde_json::to_string(&thread.launch)?;
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO yolop_threads
                    (thread_id, coordinator_session_id, workspace_root, launch_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(thread_id) DO NOTHING",
                params![
                    thread.thread_id.to_string(),
                    thread.coordinator.to_string(),
                    thread.workspace_root.display().to_string(),
                    launch,
                    now_ms(),
                ],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub(crate) fn thread(&self, thread_id: SessionId) -> anyhow::Result<Option<ThreadRecord>> {
        let row = self.db.with_conn(|conn| {
            conn.query_row(
                "SELECT thread_id, coordinator_session_id, workspace_root, launch_json
                 FROM yolop_threads WHERE thread_id = ?1",
                params![thread_id.to_string()],
                thread_row,
            )
            .optional()
        })?;
        row.map(parse_thread_row).transpose()
    }

    /// Threads of `coordinator` with messages waiting and no live host.
    fn threads_needing_host(&self, coordinator: SessionId) -> anyhow::Result<Vec<ThreadRecord>> {
        let now = now_ms();
        let rows = self.db.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT t.thread_id, t.coordinator_session_id, t.workspace_root, t.launch_json
                 FROM yolop_threads t
                 WHERE t.coordinator_session_id = ?1
                   AND t.spawned_at_ms < ?2
                   AND EXISTS (SELECT 1 FROM yolop_session_inbox i
                               WHERE i.target_session_id = t.thread_id
                                 AND i.acked_at_ms IS NULL AND i.available_at_ms <= ?3)
                   AND NOT EXISTS (SELECT 1 FROM yolop_session_presence p
                                   WHERE p.session_id = t.thread_id
                                     AND p.heartbeat_at_ms >= ?4)",
            )?;
            let rows = statement.query_map(
                params![
                    coordinator.to_string(),
                    now - THREAD_SPAWN_BACKOFF_MS,
                    now,
                    now - PRESENCE_LEASE_MS,
                ],
                thread_row,
            )?;
            rows.collect::<Result<Vec<_>, _>>()
        })?;
        rows.into_iter().map(parse_thread_row).collect()
    }

    /// Threads of `coordinator` with no live host and nothing waiting, past
    /// their spawn backoff: candidates for a crash restart.
    fn hostless_threads(&self, coordinator: SessionId) -> anyhow::Result<Vec<ThreadRecord>> {
        let now = now_ms();
        let rows = self.db.with_conn(|conn| {
            let mut statement = conn.prepare(
                "SELECT t.thread_id, t.coordinator_session_id, t.workspace_root, t.launch_json
                 FROM yolop_threads t
                 WHERE t.coordinator_session_id = ?1
                   AND t.spawned_at_ms < ?2
                   AND NOT EXISTS (SELECT 1 FROM yolop_session_inbox i
                                   WHERE i.target_session_id = t.thread_id
                                     AND i.acked_at_ms IS NULL)
                   AND NOT EXISTS (SELECT 1 FROM yolop_session_presence p
                                   WHERE p.session_id = t.thread_id
                                     AND p.heartbeat_at_ms >= ?3)",
            )?;
            let rows = statement.query_map(
                params![
                    coordinator.to_string(),
                    now - THREAD_SPAWN_BACKOFF_MS,
                    now - PRESENCE_LEASE_MS,
                ],
                thread_row,
            )?;
            rows.collect::<Result<Vec<_>, _>>()
        })?;
        rows.into_iter().map(parse_thread_row).collect()
    }

    /// Claim the right to start `thread`'s host now. False while another start
    /// is still inside its backoff window.
    fn claim_spawn(&self, thread_id: SessionId) -> anyhow::Result<bool> {
        let now = now_ms();
        let changed = self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE yolop_threads SET spawned_at_ms = ?2
                 WHERE thread_id = ?1 AND spawned_at_ms < ?3",
                params![thread_id.to_string(), now, now - THREAD_SPAWN_BACKOFF_MS],
            )
        })?;
        Ok(changed == 1)
    }
}

type ThreadRow = (String, String, String, String);

fn thread_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn parse_thread_row(
    (thread_id, coordinator, workspace_root, launch): ThreadRow,
) -> anyhow::Result<ThreadRecord> {
    Ok(ThreadRecord {
        thread_id: thread_id.parse()?,
        coordinator: coordinator.parse()?,
        workspace_root: PathBuf::from(workspace_root),
        launch: serde_json::from_str(&launch)?,
    })
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
        {
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
        let task = tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
            let mut inbox = tokio::time::interval(INBOX_INTERVAL);
            let mut supervise = tokio::time::interval(SUPERVISE_INTERVAL);
            let mut tracker = DeliveryTracker::default();
            let mut events_open = true;
            let mut yielding = false;
            loop {
                tokio::select! {
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
    if let Some(dir) = store.sessions_dir.as_deref() {
        let stopped = crate::exec::group_ledger::reap_dead_hosts(dir);
        if stopped > 0 {
            tracing::info!(stopped, "stopped commands left by a dead thread host");
        }
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
    use everruns::local::LocalSessionTaskRegistry;
    use everruns_core::session_task::{CreateSessionTask, TaskLinks, TaskWakePolicy};
    use everruns_core::{SessionTaskState, SessionTaskUpdate};
    use std::sync::Mutex;

    fn test_store() -> (CoordinationStore, Arc<LocalSessionTaskRegistry>) {
        let db = SqliteDb::open_in_memory().unwrap();
        let store = CoordinationStore::new(db.clone()).unwrap();
        let tasks = Arc::new(LocalSessionTaskRegistry::new(db).unwrap());
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
        store
            .db
            .with_conn(|conn| {
                conn.execute("UPDATE yolop_session_inbox SET available_at_ms = 0", [])?;
                Ok(())
            })
            .unwrap();
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
        store
            .db
            .with_conn(|conn| {
                conn.execute("UPDATE yolop_threads SET spawned_at_ms = 0", [])?;
                Ok(())
            })
            .unwrap();
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

//! Read Yolop's saved format before handing ownership to an Engine authority.
//!
//! This reader never starts a legacy runtime or changes the source. Import
//! needs the complete journal, including abandoned branches, rather than the
//! tolerant, active-branch projection used to render an older session.

use anyhow::{Context, Result, bail};
use everruns::hosting::{
    AgentBuild, ConversationLineage, CutoverOptions, CutoverReport, HistoryBranch, SequenceRange,
};
use everruns_contracts::typed_id::SessionId;
use everruns_core::{CompactionCheckpoint, Event};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use super::compaction_checkpoint::{LegacyCompactionAttempt, read_legacy_compaction};
use super::session_log::{
    SessionFileLease, SessionWorkspaceMetadata, lock_session, session_log_path,
};
use crate::session_state::checkpoint::{LegacyConversation, read_legacy_conversation};

// Matches the SQLite authority's maximum imported event envelope.
const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = everruns_core::host::events::MAX_JSONL_RECOVERY_BYTES;
const MAX_SOURCE_RECORDS: usize = everruns_core::host::events::MAX_JSONL_RECOVERY_EVENTS;

/// A decoded source, not another persisted session format or authority.
pub struct LegacySessionSource {
    pub session_id: SessionId,
    pub events: Vec<Event>,
    pub conversation: LegacyConversation,
    pub checkpoints: Vec<CompactionCheckpoint>,
    pub compaction_attempts: Vec<LegacyCompactionAttempt>,
    pub workspace: Option<SessionWorkspaceMetadata>,
    /// Digest of all source files, including workspace/checkpoint metadata.
    pub fingerprint: String,
    children: Vec<LegacyChildSource>,
    journal: Vec<Event>,
    journal_digest: [u8; 32],
    source_dir: std::path::PathBuf,
    lease: Option<SessionFileLease>,
}

struct LegacyChildSource {
    id: SessionId,
    parent: SessionId,
    events: Vec<Event>,
}

impl LegacySessionSource {
    /// Translate the product's timeline into immutable authority branches.
    /// Each turn boundary is retained, including siblings that were abandoned
    /// before the current redo point. Workspace snapshots stay product metadata.
    pub fn lineage(&self) -> Result<ConversationLineage> {
        let conversation = &self.conversation;
        let maximum = self
            .events
            .last()
            .and_then(|event| event.sequence)
            .unwrap_or(0);
        let baseline_end = conversation.baseline_end.min(maximum);
        let baseline = if baseline_end > 0 {
            vec![SequenceRange::new(1, baseline_end)]
        } else {
            Vec::new()
        };
        let branch_id = |id: &str| format!("turn:{id}");
        let active = conversation
            .pending
            .as_deref()
            .or(conversation.head.as_deref())
            .map(branch_id)
            .unwrap_or_else(|| "baseline".into());
        let redo = conversation
            .redo
            .as_ref()
            .and_then(|redo| redo.head.as_deref())
            .map(branch_id);
        let parents: HashSet<_> = conversation
            .checkpoints
            .iter()
            .filter_map(|node| node.parent.as_deref())
            .collect();
        let nodes: HashMap<_, _> = conversation
            .checkpoints
            .iter()
            .map(|node| (node.id.as_str(), node))
            .collect();
        let mut branches = vec![HistoryBranch {
            id: "baseline".into(),
            ranges: baseline.clone(),
        }];
        // Construct only retained leaves and selected heads. Building every
        // ancestor's complete path first grows quadratically on linear history.
        for checkpoint in conversation.checkpoints.iter().filter(|node| {
            !parents.contains(node.id.as_str())
                || branch_id(&node.id) == active
                || Some(branch_id(&node.id)) == redo
        }) {
            let mut path = vec![checkpoint];
            let mut parent = checkpoint.parent.as_deref();
            while let Some(id) = parent {
                let node = *nodes
                    .get(id)
                    .context("migration timeline parent is missing")?;
                path.push(node);
                parent = node.parent.as_deref();
            }
            let mut ranges = baseline.clone();
            for node in path.into_iter().rev() {
                let last = node.event_end.unwrap_or(maximum).min(maximum);
                if node.event_start <= last {
                    if let Some(previous) = ranges.last_mut()
                        && previous.last.checked_add(1) == Some(node.event_start)
                    {
                        previous.last = last;
                    } else {
                        ranges.push(SequenceRange::new(node.event_start, last));
                    }
                }
            }
            branches.push(HistoryBranch {
                id: branch_id(&checkpoint.id),
                ranges,
            });
        }
        let mut lineage = ConversationLineage::new(active, branches);
        for checkpoint in &self.checkpoints {
            // The producing turn is the unique owner of this journal position.
            // An ancestor prefix is compatible on all of its descendants.
            let sequence = i32::try_from(checkpoint.source_sequence)?;
            let branch = lineage
                .branches
                .iter()
                .find(|branch| {
                    branch
                        .ranges
                        .iter()
                        .any(|range| range.first <= sequence && sequence <= range.last)
                })
                .context("compaction checkpoint has no retained conversation lineage")?
                .id
                .clone();
            lineage = lineage.with_checkpoint(branch, checkpoint.clone());
        }
        Ok(lineage)
    }

    /// Import a frozen source into registered hosting. The legacy Agent is a
    /// source descriptor only: no runtime or legacy Engine session is started.
    #[allow(deprecated)]
    pub async fn cut_over(
        &self,
        engine: &everruns::Engine,
        build: &AgentBuild,
        backends: everruns_core::host::HostBackends,
        mut metadata: serde_json::Value,
    ) -> Result<CutoverReport> {
        // Child events used to share the root's physical journal and sequence
        // allocator. Archive the original source before rebuilding root exports.
        self.retain_source_journal()?;
        let template = backends
            .session_store
            .get_session(self.session_id)
            .await?
            .context("migration source session is missing")?;
        for child in &self.children {
            let mut record = template.clone();
            record.id = child.id;
            record.parent_session_id = Some(child.parent);
            backends.session_store.add_session(record).await?;
            let source = everruns::Agent::builder()
                .instructions("Saved Yolop child session migration source.")
                .model(everruns::Model::simulated(
                    "migration source must never execute",
                ))
                .backends(
                    backends
                        .clone()
                        .with_event_log(std::sync::Arc::new(SourceJournal {
                            session: child.id,
                            events: child.events.clone(),
                        })),
                )
                .build()?;
            engine.cut_over_legacy_session_with_metadata(child.id, &source, build, serde_json::json!({
                "parent_session_id": child.parent,
                "yolop_migration": { "format":"yolop-jsonl-v1", "fingerprint":self.fingerprint },
            })).await?;
        }
        let source = everruns::Agent::builder()
            .instructions("Saved Yolop session migration source.")
            .model(everruns::Model::simulated(
                "migration source must never execute",
            ))
            .backends(backends.with_event_log(std::sync::Arc::new(SourceJournal {
                session: self.session_id,
                events: self.events.clone(),
            })))
            .build()?;
        if metadata.is_null() {
            metadata = serde_json::json!({});
        }
        metadata["yolop_migration"] = serde_json::json!({
            "format": "yolop-jsonl-v1", "fingerprint": self.fingerprint,
            "events": self.events.len(), "workspace_checkpoints": self.conversation.checkpoints.len(),
            "compaction_attempts": self.compaction_attempts.len(),
        });
        let options = CutoverOptions::new()
            .lineage(self.lineage()?)
            .metadata(metadata);
        Ok(engine
            .cut_over_legacy_session_with_options(self.session_id, &source, build, options)
            .await?)
    }

    /// Retain the same exclusive lease used by Yolop execution until this
    /// source is dropped. Malformed or foreign records refuse migration;
    /// replay's best-effort skipping is unsuitable for an ownership transfer.
    pub fn read(session_dir: &Path, session_id: SessionId) -> Result<Self> {
        Self::read_with_links(session_dir, session_id, HashMap::new())
    }

    pub async fn read_for_runtime(
        session_dir: &Path,
        session_id: SessionId,
        sessions_dir: &Path,
    ) -> Result<Self> {
        use everruns_core::SessionTaskRegistry;
        let db_path =
            everruns::local::LocalProfile::new(sessions_dir.join("everruns-local")).db_path();
        let mut parents = HashMap::new();
        if db_path.exists() {
            let tasks = everruns::local::LocalSessionTaskRegistry::new(
                everruns::local::SqliteDb::open(db_path)?,
            )?;
            let mut queue = std::collections::VecDeque::from([session_id]);
            while let Some(parent) = queue.pop_front() {
                for task in tasks.list(parent, None).await? {
                    if let Some(child) = task.links.child_session_id {
                        anyhow::ensure!(
                            child != session_id,
                            "migration child linkage contains a cycle"
                        );
                        if let Some(previous) = parents.insert(child, parent) {
                            anyhow::ensure!(
                                previous == parent,
                                "migration child has conflicting parents"
                            );
                        } else {
                            anyhow::ensure!(
                                parents.len() <= MAX_SOURCE_RECORDS,
                                "migration has too many child sessions"
                            );
                            queue.push_back(child);
                        }
                    }
                }
            }
        }
        Self::read_with_links(session_dir, session_id, parents)
    }

    fn read_with_links(
        session_dir: &Path,
        session_id: SessionId,
        mut parents: HashMap<SessionId, SessionId>,
    ) -> Result<Self> {
        if !session_dir.is_dir() {
            bail!("migration source session does not exist");
        }
        let lease = lock_session(session_dir)?;
        let mut digest = Sha256::new();
        let mut events: Vec<Event> = read_records(&session_log_path(session_dir), &mut digest)?;
        let journal_digest = digest.clone().finalize().into();
        let mut sequence = 0i32;
        let mut ids = HashSet::new();
        for event in &mut events {
            if !ids.insert(event.id) {
                bail!("migration journal contains a duplicate event identity");
            }
            let current = match event.sequence {
                Some(current) => current,
                None => sequence
                    .checked_add(1)
                    .context("migration event sequence is exhausted")?,
            };
            if current <= sequence {
                bail!("migration journal event sequences are not strictly increasing");
            }
            event.sequence = Some(current);
            sequence = current;
        }
        // Successful subagent results are another durable parent link, including
        // older formats which predate the persistent task registry.
        loop {
            let mut changed = false;
            for event in &events {
                if event.session_id != session_id && !parents.contains_key(&event.session_id) {
                    continue;
                }
                let everruns_core::EventData::ToolCompleted(tool) = &event.data else {
                    continue;
                };
                if tool.tool_name != "subagent" || !tool.success {
                    continue;
                }
                for part in tool.result.iter().flatten() {
                    let everruns_core::ContentPart::Text(text) = part else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text.text) else {
                        continue;
                    };
                    let Some(child) = value
                        .get("subagent_id")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|id| id.parse::<SessionId>().ok())
                    else {
                        continue;
                    };
                    anyhow::ensure!(
                        child != session_id,
                        "migration child linkage contains a cycle"
                    );
                    if let Some(previous) = parents.insert(child, event.session_id) {
                        anyhow::ensure!(
                            previous == event.session_id,
                            "migration child has conflicting parents"
                        );
                    } else {
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        anyhow::ensure!(
            events
                .iter()
                .all(|event| event.session_id == session_id
                    || parents.contains_key(&event.session_id)),
            "migration journal contains an event for another session without a retained parent link"
        );
        for child in parents.keys() {
            let mut visited = HashSet::new();
            let mut cursor = *child;
            while cursor != session_id {
                anyhow::ensure!(
                    visited.insert(cursor),
                    "migration child linkage contains a cycle"
                );
                cursor = *parents
                    .get(&cursor)
                    .context("migration child parent is missing")?;
            }
        }
        let journal = events;
        let events: Vec<_> = journal
            .iter()
            .filter(|event| event.session_id == session_id)
            .cloned()
            .collect();
        let mut children: Vec<_> = parents
            .into_iter()
            .map(|(id, parent)| LegacyChildSource {
                id,
                parent,
                events: journal
                    .iter()
                    .filter(|event| event.session_id == id)
                    .cloned()
                    .collect(),
            })
            .collect();
        children.sort_by_key(|child| child.id.to_string());
        let conversation = read_legacy_conversation(session_dir, sequence, &mut digest)?;
        let (checkpoints, compaction_attempts) =
            read_legacy_compaction(session_dir, session_id, sequence, &mut digest)?;
        let workspace_path = session_dir.join("workspace.json");
        let workspace = match read_source_file(&workspace_path)? {
            Some(file) => {
                let bytes = read_bounded(file, MAX_RECORD_BYTES)?;
                digest_source(&mut digest, &workspace_path, Some(&bytes));
                Some(serde_json::from_slice(&bytes).context("parse migration workspace metadata")?)
            }
            None => {
                digest_source(&mut digest, &workspace_path, None);
                None
            }
        };
        Ok(Self {
            session_id,
            events,
            conversation,
            checkpoints,
            compaction_attempts,
            workspace,
            children,
            journal,
            journal_digest,
            source_dir: session_dir.to_path_buf(),
            fingerprint: digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            lease: Some(lease),
        })
    }

    fn retain_source_journal(&self) -> Result<()> {
        // Archive exact source bytes, including fields a newer Event schema may
        // not understand. The native journal remains the execution authority.
        let path = self.source_dir.join("legacy-events.jsonl");
        if let Some(file) = read_source_file(&path)? {
            return self.verify_and_copy_journal(file, None);
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let temporary = self.source_dir.join(format!(
            ".legacy-events-{}.tmp",
            everruns_contracts::typed_id::EventId::new()
        ));
        let mut archive = options.open(&temporary)?;
        let source = read_source_file(&session_log_path(&self.source_dir))?
            .context("migration source journal disappeared")?;
        let result = self.verify_and_copy_journal(source, Some(&mut archive));
        if let Err(error) = result {
            drop(archive);
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        archive.sync_all()?;
        std::fs::rename(temporary, path)?;
        #[cfg(unix)]
        File::open(&self.source_dir)?.sync_all()?;
        Ok(())
    }

    fn verify_and_copy_journal(
        &self,
        mut source: File,
        mut archive: Option<&mut File>,
    ) -> Result<()> {
        use std::io::Write;
        let mut digest = Sha256::new();
        let mut length = 0u64;
        let mut buffer = [0u8; 8192];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            length += count as u64;
            anyhow::ensure!(
                length <= MAX_SOURCE_BYTES as u64,
                "migration source exceeds the authority recovery byte limit"
            );
            digest.update(&buffer[..count]);
            if let Some(file) = archive.as_mut() {
                file.write_all(&buffer[..count])?;
            }
        }
        let mut fingerprint = Sha256::new();
        digest_source(
            &mut fingerprint,
            Path::new("events.jsonl"),
            Some(&digest.finalize()),
        );
        fingerprint.update(length.to_be_bytes());
        let actual: [u8; 32] = fingerprint.finalize().into();
        anyhow::ensure!(
            actual == self.journal_digest,
            "migration source archive differs from the frozen journal"
        );
        Ok(())
    }

    /// Transfer the existing file lease to the session lifetime without an
    /// unlocked interval between source validation and authority import.
    pub(crate) fn take_lease(&mut self) -> SessionFileLease {
        self.lease
            .take()
            .expect("migration lease already transferred")
    }

    /// Bounded preflight output contains no retained messages or opaque
    /// provider payloads. A valid source is not yet a committed cutover.
    pub fn summary(&self) -> serde_json::Value {
        let compaction_scopes: std::collections::BTreeSet<_> = self
            .compaction_attempts
            .iter()
            .map(|attempt| {
                (
                    &attempt.provider_type,
                    &attempt.model,
                    attempt.attempt.source_sequence,
                )
            })
            .take(50)
            .collect();
        serde_json::json!({
            "session_id":self.session_id,
            "source_valid":true,
            "cutover_performed":false,
            "source_fingerprint":self.fingerprint,
            "event_count":self.journal.len(),
            "child_session_count":self.children.len(),
            "last_sequence":self.events.last().and_then(|event| event.sequence),
            "baseline_end":self.conversation.baseline_end,
            "checkpoint_count":self.conversation.checkpoints.len(),
            "active_head":self.conversation.head,
            "pending_head":self.conversation.pending,
            "redo_head":self.conversation.redo.as_ref().map(|redo| &redo.head),
            "active_range_count":self.conversation.active_ranges.len(),
            "compaction_checkpoint_count":self.checkpoints.len(),
            "compaction_attempt_count":self.compaction_attempts.len(),
            "compaction_attempt_scopes":compaction_scopes,
            "compaction_attempt_scopes_truncated":self.compaction_attempts.len()>50,
            "has_workspace_metadata":self.workspace.is_some(),
        })
    }
}

/// Read-only, identity-preserving view of the journal held under the file lease.
/// Refusing appends makes accidental use as an execution backend fail closed.
struct SourceJournal {
    session: SessionId,
    events: Vec<Event>,
}

#[async_trait::async_trait]
impl everruns_core::host::EventReader for SourceJournal {
    async fn read_page(
        &self,
        request: everruns_core::host::EventReadRequest,
    ) -> std::result::Result<everruns_core::host::EventPage, everruns_core::host::EventLogError>
    {
        use everruns_core::host::{EventCursor, EventLogError, EventPage};
        if request.session_id() != self.session {
            return EventPage::new(Vec::new(), None, 0);
        }
        let head = self
            .events
            .last()
            .and_then(|event| event.sequence)
            .unwrap_or(0);
        let (after, snapshot) = match request.cursor() {
            Some(cursor) if cursor.session_id() != self.session => {
                return Err(EventLogError::CrossSessionCursor {
                    detail: "migration cursor belongs to another session".into(),
                });
            }
            Some(cursor) => (
                cursor.after_sequence(),
                cursor.snapshot_high_watermark().unwrap_or(head),
            ),
            None => (0, head),
        };
        if snapshot > head {
            return Err(EventLogError::InvalidRead {
                detail: "migration cursor exceeds source journal".into(),
            });
        }
        let events: Vec<_> = self
            .events
            .iter()
            .filter(|event| {
                event
                    .sequence
                    .is_some_and(|sequence| after < sequence && sequence <= snapshot)
            })
            .take(request.limit().get())
            .cloned()
            .collect();
        let next = events
            .last()
            .and_then(|event| event.sequence)
            .filter(|last| *last < snapshot)
            .map(|last| EventCursor::continuation(self.session, last, snapshot))
            .transpose()?;
        EventPage::new(events, next, snapshot)
    }
}

#[async_trait::async_trait]
impl everruns_core::host::EventLog for SourceJournal {
    async fn append(
        &self,
        _: everruns_core::EventRequest,
    ) -> std::result::Result<Event, everruns_core::host::EventLogError> {
        Err(everruns_core::host::EventLogError::InvalidAppend {
            detail: "migration source is read-only".into(),
        })
    }
    async fn append_conditional(
        &self,
        request: everruns_core::EventRequest,
        _: Option<i32>,
    ) -> std::result::Result<Event, everruns_core::host::EventLogError> {
        self.append(request).await
    }
    fn durability(&self) -> everruns_core::host::EventDurability {
        everruns_core::host::EventDurability::Volatile
    }
}

fn read_bounded(file: File, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        bail!("migration source record exceeds the authority size limit");
    }
    Ok(bytes)
}

fn read_source_file(path: &Path) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open migration source"),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("migration source must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            bail!("migration source must have one hard link");
        }
    }
    Ok(Some(file))
}

fn digest_source(digest: &mut Sha256, path: &Path, bytes: Option<&[u8]>) {
    let name = path
        .file_name()
        .expect("source has a filename")
        .as_encoded_bytes();
    digest.update((name.len() as u64).to_be_bytes());
    digest.update(name);
    digest.update([u8::from(bytes.is_some())]);
    if let Some(bytes) = bytes {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
}

pub(crate) fn read_records<T: DeserializeOwned>(
    path: &Path,
    digest: &mut Sha256,
) -> Result<Vec<T>> {
    let Some(file) = read_source_file(path)? else {
        digest_source(digest, path, None);
        return Ok(Vec::new());
    };
    if file.metadata()?.len() > MAX_SOURCE_BYTES as u64 {
        bail!("migration source exceeds the authority recovery byte limit");
    }
    // Hash the exact bytes, including blank lines and a final newline. A
    // source edit must conflict even when its decoded values happen to agree.
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut line = Vec::new();
    let mut file_digest = Sha256::new();
    let mut length = 0u64;
    let mut number = 0usize;
    loop {
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_RECORD_BYTES as u64 + 2)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        number += 1;
        if line.len() - usize::from(line.ends_with(b"\n")) > MAX_RECORD_BYTES {
            bail!("migration source line {number} exceeds the authority size limit");
        }
        if length + line.len() as u64 > MAX_SOURCE_BYTES as u64 {
            bail!("migration source exceeds the authority recovery byte limit");
        }
        file_digest.update(&line);
        length += line.len() as u64;
        if !line.iter().all(u8::is_ascii_whitespace) {
            if records.len() >= MAX_SOURCE_RECORDS {
                bail!("migration source exceeds the authority recovery record limit");
            }
            records.push(serde_json::from_slice(&line).with_context(|| {
                format!("parse migration source {} line {number}", path.display())
            })?);
        }
    }
    digest_source(digest, path, Some(&file_digest.finalize()));
    digest.update(length.to_be_bytes());
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_contracts::{CompactOutputItem, ProviderOpaqueContext};
    use everruns_core::{
        CompactionCheckpointPayload, EventContext, InputMessageData, RuntimeMessage,
    };
    use serde_json::{Value, json};

    fn write_records(path: &Path, records: &[Value]) {
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, record).unwrap();
            bytes.push(b'\n');
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn event(session: SessionId, sequence: Option<i32>, text: &str) -> Event {
        let mut event = Event::new(
            session,
            EventContext::default(),
            InputMessageData::new(RuntimeMessage::user(text)),
        );
        event.sequence = sequence;
        event
    }

    fn journal(path: &Path, events: &[Event]) {
        write_records(
            path,
            &events.iter().map(|event| json!(event)).collect::<Vec<_>>(),
        );
    }

    fn timeline() -> Vec<Value> {
        let timestamp = "2026-10-19T00:00:00Z";
        let mut records = vec![json!({
            "type":"initialized", "baseline_end":0, "created_at":timestamp,
        })];
        for (id, parent, start) in [("A", None, 1), ("B", Some("A"), 3)] {
            records.push(json!({
                "type":"checkpoint_created", "id":id, "parent":parent,
                "prompt":format!("request {id}"), "event_start":start,
                "snapshot":{"tree":format!("tree-{id}"),"reference":format!("refs/yolop/checkpoints/session/{id}")},
                "snapshot_error":null, "created_at":timestamp,
            }));
            records.push(json!({
                "type":"turn_completed", "checkpoint_id":id, "event_end":start+1,
                "success":true, "completed_at":timestamp,
            }));
        }
        records.push(json!({
            "type":"head_moved", "from":"B", "to":"A", "redo_head":"B",
            "redo_snapshot":{"tree":"tree-redo","reference":"refs/yolop/checkpoints/session/redo"},
            "moved_at":timestamp,
        }));
        records
    }

    fn compact(session: SessionId, sequence: i32) -> Value {
        let payload = CompactionCheckpointPayload::ProviderOpaque {
            context: ProviderOpaqueContext::OpenResponsesCompact {
                reasoning_state: None,
                output: vec![CompactOutputItem::Compaction {
                    encrypted_content: format!("opaque-{sequence}"),
                }],
            },
        };
        json!({"record_type":"checkpoint", "record":{
            "id":format!("00000000-0000-7000-8000-{sequence:012}"),
            "session_id":session, "source_sequence":sequence, "provider_type":"openai",
            "model":"test", "format_version":1, "payload":payload,
        }})
    }

    #[test]
    fn retains_abandoned_history_redo_and_every_compaction_without_changing_source() {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let events: Vec<_> = (1..=4)
            .map(|i| event(session, Some(i), &format!("message {i}")))
            .collect();
        journal(&temp.path().join("events.jsonl"), &events);
        write_records(&temp.path().join("timeline.jsonl"), &timeline());
        write_records(
            &temp.path().join("compaction-checkpoints.jsonl"),
            &[compact(session, 2), compact(session, 4)],
        );
        let before: Vec<_> = [
            "events.jsonl",
            "timeline.jsonl",
            "compaction-checkpoints.jsonl",
        ]
        .map(|name| std::fs::read(temp.path().join(name)).unwrap())
        .into();

        let source = LegacySessionSource::read(temp.path(), session).unwrap();
        assert_eq!(
            json!(source.events),
            json!(events),
            "all original event/message identities survive"
        );
        assert_eq!(source.conversation.active_ranges, [(1, 2)]);
        assert_eq!(source.conversation.head.as_deref(), Some("A"));
        let redo = source.conversation.redo.as_ref().unwrap();
        assert_eq!(redo.head.as_deref(), Some("B"));
        assert_eq!(redo.workspace_tree.as_deref(), Some("tree-redo"));
        assert_eq!(source.conversation.checkpoints.len(), 2);
        let abandoned = source
            .conversation
            .checkpoints
            .iter()
            .find(|node| node.id == "B")
            .unwrap();
        assert_eq!(abandoned.parent.as_deref(), Some("A"));
        assert_eq!((abandoned.event_start, abandoned.event_end), (3, Some(4)));
        assert_eq!(abandoned.workspace_tree.as_deref(), Some("tree-B"));
        assert_eq!(
            source
                .checkpoints
                .iter()
                .map(|checkpoint| checkpoint.source_sequence)
                .collect::<Vec<_>>(),
            [2, 4]
        );
        let fingerprint = source.fingerprint.clone();
        assert!(
            LegacySessionSource::read(temp.path(), session).is_err(),
            "a second reader cannot race the ownership transfer"
        );
        drop(source);
        assert_eq!(
            fingerprint,
            LegacySessionSource::read(temp.path(), session)
                .unwrap()
                .fingerprint
        );
        for (name, bytes) in [
            "events.jsonl",
            "timeline.jsonl",
            "compaction-checkpoints.jsonl",
        ]
        .into_iter()
        .zip(before)
        {
            assert_eq!(std::fs::read(temp.path().join(name)).unwrap(), bytes);
        }
        assert!(!temp.path().join("workspace.json").exists());
    }

    #[test]
    fn reconstructs_the_selected_fork_and_retains_its_abandoned_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        journal(
            &temp.path().join("events.jsonl"),
            &(1..=6)
                .map(|i| event(session, Some(i), "input"))
                .collect::<Vec<_>>(),
        );
        let mut records = timeline();
        records.extend([
            json!({"type":"redo_cleared","cleared_at":"2026-10-19T00:00:00Z"}),
            json!({"type":"checkpoint_created","id":"C","parent":"A","prompt":"replacement", "event_start":5,"snapshot":null,"snapshot_error":null,"created_at":"2026-10-19T00:00:00Z"}),
            json!({"type":"turn_completed","checkpoint_id":"C","event_end":6,"success":true,"completed_at":"2026-10-19T00:00:00Z"}),
        ]);
        write_records(&temp.path().join("timeline.jsonl"), &records);
        let source = LegacySessionSource::read(temp.path(), session).unwrap();
        assert_eq!(source.conversation.active_ranges, [(1, 2), (5, 6)]);
        assert_eq!(source.conversation.checkpoints.len(), 3);
        assert_eq!(source.conversation.head.as_deref(), Some("C"));
        assert!(source.conversation.redo.is_none());
        assert_eq!(source.events.len(), 6);
        let lineage = source.lineage().unwrap();
        assert_eq!(lineage.active, "turn:C");
        assert_eq!(
            lineage
                .branches
                .iter()
                .find(|branch| branch.id == "turn:B")
                .unwrap()
                .ranges,
            [SequenceRange::new(1, 4)]
        );
        assert_eq!(
            lineage
                .branches
                .iter()
                .find(|branch| branch.id == "turn:C")
                .unwrap()
                .ranges,
            [SequenceRange::new(1, 2), SequenceRange::new(5, 6)]
        );
    }

    #[tokio::test]
    async fn mixed_child_history_import_is_lossless_and_retryable() {
        use everruns::hosting::{SessionHosting, SqliteHosting};
        use everruns_core::host::SessionBuilder;
        let temp = tempfile::tempdir().unwrap();
        let root = SessionId::new();
        let child = SessionId::new();
        let nested = SessionId::new();
        let events = [
            event(root, Some(1), "parent"),
            event(child, Some(2), "child"),
            event(nested, Some(3), "nested"),
            event(root, Some(4), "parent again"),
        ];
        let mut raw: Vec<Value> = events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();
        raw[0]["retired_extension_field"] = json!({"original":"must survive decoding"});
        write_records(&temp.path().join("events.jsonl"), &raw);
        let text = std::fs::read_to_string(temp.path().join("events.jsonl")).unwrap();
        std::fs::write(temp.path().join("events.jsonl"), format!("\n{text}\n")).unwrap();
        let original_bytes = std::fs::read(temp.path().join("events.jsonl")).unwrap();
        assert!(LegacySessionSource::read(temp.path(), root).is_err());
        let source = LegacySessionSource::read_with_links(
            temp.path(),
            root,
            HashMap::from([(child, root), (nested, child)]),
        )
        .unwrap();
        assert_eq!(source.summary()["event_count"], 4);
        let profile = SqliteHosting::open(temp.path().join("authority.sqlite")).unwrap();
        let build = AgentBuild::new("yolop", "family-test");
        let agent = everruns::Agent::builder()
            .model(everruns::Model::simulated("reply"))
            .instructions("Answer briefly.")
            .build()
            .unwrap();
        let engine = everruns::Engine::builder()
            .hosting(profile.clone())
            .register(build.clone(), agent)
            .build();
        let backends = everruns_core::host::HostBackends::in_memory();
        backends
            .session_store
            .add_session(
                SessionBuilder::new(everruns_contracts::typed_id::HarnessId::new())
                    .id(root)
                    .build(),
            )
            .await
            .unwrap();
        source
            .cut_over(&engine, &build, backends.clone(), serde_json::Value::Null)
            .await
            .unwrap();
        assert!(
            source
                .cut_over(&engine, &build, backends, serde_json::Value::Null)
                .await
                .unwrap()
                .replayed
        );
        for id in [root, child, nested] {
            let mut raw = Vec::new();
            profile.export_jsonl(id, &mut raw).unwrap();
            let imported: Vec<Event> = String::from_utf8(raw)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let original: Vec<_> = events
                .iter()
                .filter(|event| event.session_id == id)
                .collect();
            assert_eq!(
                imported
                    .iter()
                    .map(|event| (event.id, event.sequence))
                    .collect::<Vec<_>>(),
                original
                    .iter()
                    .map(|event| (event.id, event.sequence))
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(
            profile.status(nested).await.unwrap().binding.metadata["parent_session_id"],
            json!(child)
        );
        profile
            .rebuild_jsonl_export(root, temp.path().join("events.jsonl"))
            .unwrap();
        let archived: Vec<Event> =
            read_records(&temp.path().join("legacy-events.jsonl"), &mut Sha256::new()).unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("legacy-events.jsonl")).unwrap(),
            original_bytes
        );
        assert_eq!(
            archived.iter().map(|event| event.id).collect::<Vec<_>>(),
            events.iter().map(|event| event.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn long_linear_history_fits_the_authority_branch_budget() {
        let temp = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let events: Vec<_> = (1..=2200)
            .map(|sequence| event(id, Some(sequence), "saved input"))
            .collect();
        journal(&temp.path().join("events.jsonl"), &events);
        let mut records = vec![timeline()[0].clone()];
        for index in 0..1100 {
            records.push(
                json!({"type":"checkpoint_created", "id":format!("C{index}"),
                "parent":if index == 0 { None } else { Some(format!("C{}", index-1)) },
                "prompt":"saved input", "event_start":index*2+1, "snapshot":null,
                "snapshot_error":null, "created_at":"2026-10-25T00:00:00Z"}),
            );
            records.push(
                json!({"type":"turn_completed", "checkpoint_id":format!("C{index}"),
                "event_end":index*2+2, "success":true, "completed_at":"2026-10-25T00:00:00Z"}),
            );
        }
        write_records(&temp.path().join("timeline.jsonl"), &records);
        let source = LegacySessionSource::read(temp.path(), id).unwrap();
        let lineage = source.lineage().unwrap();
        assert_eq!(lineage.branches.len(), 2);
        assert_eq!(
            lineage
                .branches
                .iter()
                .find(|branch| branch.id == lineage.active)
                .unwrap()
                .ranges,
            [SequenceRange::new(1, 2200)]
        );
        assert_eq!(source.conversation.checkpoints.len(), 1100);
    }

    #[tokio::test]
    async fn engine_import_preserves_identity_branches_compaction_and_restart() {
        use everruns::hosting::{SessionHosting, SqliteHosting};
        use everruns::{Agent, Engine, Model};
        use everruns_contracts::typed_id::HarnessId;
        use everruns_core::host::SessionBuilder;
        let temp = tempfile::tempdir().unwrap();
        let agent = || {
            Agent::builder()
                .instructions("Answer briefly.")
                .model(Model::simulated("reply"))
                .build()
                .unwrap()
        };
        let build = AgentBuild::new("yolop", "migration-test-v1");
        let fixture_profile = SqliteHosting::open(temp.path().join("fixture.db")).unwrap();
        let fixture = Engine::builder()
            .hosting(fixture_profile.clone())
            .register(build.clone(), agent())
            .build();
        let id = SessionId::new();
        fixture.create_registered(id, &build).await.unwrap();
        let session = fixture.resume(id).await.unwrap();
        assert!(session.run("alpha").await.unwrap().success);
        let mut first = Vec::new();
        fixture_profile.export_jsonl(id, &mut first).unwrap();
        let first_events: Vec<Event> = String::from_utf8(first)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let first_end = first_events.last().unwrap().sequence.unwrap();
        assert!(session.run("beta").await.unwrap().success);
        let mut raw = Vec::new();
        fixture_profile.export_jsonl(id, &mut raw).unwrap();
        let events: Vec<Event> = String::from_utf8(raw)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let head = events.last().unwrap().sequence.unwrap();
        let source_dir = temp.path().join("saved");
        std::fs::create_dir(&source_dir).unwrap();
        journal(&source_dir.join("events.jsonl"), &events);
        let mut records = timeline();
        records[2]["event_end"] = json!(first_end);
        records[3]["event_start"] = json!(first_end + 1);
        records[4]["event_end"] = json!(head);
        write_records(&source_dir.join("timeline.jsonl"), &records);
        write_records(
            &source_dir.join("compaction-checkpoints.jsonl"),
            &[compact(id, first_end), compact(id, head)],
        );
        let source = LegacySessionSource::read(&source_dir, id).unwrap();
        let path = temp.path().join("authority.db");
        let profile = SqliteHosting::open(&path).unwrap();
        let engine = Engine::builder()
            .hosting(profile.clone())
            .register(build.clone(), agent())
            .build();
        let backends = everruns_core::host::HostBackends::in_memory();
        backends
            .session_store
            .add_session(SessionBuilder::new(HarnessId::new()).id(id).build())
            .await
            .unwrap();
        let report = source
            .cut_over(&engine, &build, backends.clone(), serde_json::Value::Null)
            .await
            .unwrap();
        assert!(!report.replayed);
        assert_eq!(report.boundary.events, events.len());
        assert!(
            source
                .cut_over(&engine, &build, backends, serde_json::Value::Null)
                .await
                .unwrap()
                .replayed
        );
        let imported = engine.resume(id).await.unwrap();
        let texts = |page: everruns::HistoryPage| {
            page.messages
                .iter()
                .map(|message| message.text())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            texts(imported.history().page().await.unwrap()),
            ["alpha", "reply"]
        );
        let reader = profile.reader(id);
        let eligible = reader
            .compaction_checkpoint_store
            .get_latest_format(id, "openai", "test", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(eligible.source_sequence, i64::from(first_end));
        assert!(imported.run("gamma").await.unwrap().success);
        assert_eq!(
            texts(imported.history().page().await.unwrap()),
            ["alpha", "reply", "gamma", "reply"]
        );
        drop(imported);
        drop(engine);
        drop(profile);
        let profile = SqliteHosting::open(&path).unwrap();
        let engine = Engine::builder()
            .hosting(profile.clone())
            .register(build.clone(), agent())
            .build();
        let lineage = engine.history_lineage(id).await.unwrap();
        engine
            .select_history_branch(
                id,
                everruns_contracts::typed_id::MessageId::new().uuid(),
                lineage.revision,
                "turn:B",
                None,
            )
            .await
            .unwrap();
        let imported = engine.resume(id).await.unwrap();
        assert_eq!(
            texts(imported.history().page().await.unwrap()),
            ["alpha", "reply", "beta", "reply"]
        );
        let eligible = profile
            .reader(id)
            .compaction_checkpoint_store
            .get_latest_format(id, "openai", "test", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(eligible.source_sequence, i64::from(head));
        let mut exported = Vec::new();
        profile.export_jsonl(id, &mut exported).unwrap();
        let exported: Vec<Event> = String::from_utf8(exported)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(json!(&exported[..events.len()]), json!(events));
    }

    #[test]
    fn normalizes_unsequenced_events_and_fingerprints_raw_source_edits() {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let path = temp.path().join("events.jsonl");
        let events = [
            event(session, None, "old"),
            event(session, Some(7), "new"),
            event(session, None, "last"),
        ];
        journal(&path, &events);
        let source = LegacySessionSource::read(temp.path(), session).unwrap();
        assert_eq!(
            source
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [Some(1), Some(7), Some(8)]
        );
        assert_eq!(source.conversation.baseline_end, 8);
        assert_eq!(source.conversation.active_ranges, [(1, 8)]);
        let fingerprint = source.fingerprint.clone();
        let normalized_events = json!(source.events);
        drop(source);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();
        let edited = LegacySessionSource::read(temp.path(), session).unwrap();
        assert_ne!(fingerprint, edited.fingerprint);
        assert_eq!(normalized_events, json!(edited.events));
    }

    #[test]
    fn refuses_corrupt_foreign_duplicate_and_out_of_order_events() {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let path = temp.path().join("events.jsonl");
        let one = event(session, Some(1), "one");
        let foreign = event(SessionId::new(), Some(2), "foreign");
        for events in [
            vec![one.clone(), foreign],
            vec![one.clone(), one.clone()],
            vec![one.clone(), event(session, Some(0), "backwards")],
        ] {
            journal(&path, &events);
            assert!(LegacySessionSource::read(temp.path(), session).is_err());
        }
        journal(&path, &[one]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"incomplete\":");
        std::fs::write(&path, &bytes).unwrap();
        assert!(LegacySessionSource::read(temp.path(), session).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "failed migration cannot repair or truncate its source"
        );
    }

    #[test]
    fn refuses_a_missing_history_parent_and_compaction_past_the_journal() {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        journal(
            &temp.path().join("events.jsonl"),
            &(1..=4)
                .map(|i| event(session, Some(i), "input"))
                .collect::<Vec<_>>(),
        );
        let mut records = timeline();
        records[1]["parent"] = json!("missing");
        write_records(&temp.path().join("timeline.jsonl"), &records);
        assert!(LegacySessionSource::read(temp.path(), session).is_err());
        write_records(&temp.path().join("timeline.jsonl"), &timeline());
        write_records(
            &temp.path().join("compaction-checkpoints.jsonl"),
            &[compact(session, 5)],
        );
        assert!(LegacySessionSource::read(temp.path(), session).is_err());
    }

    #[test]
    fn preserves_an_interrupted_turn_and_compaction_retry_state() {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        journal(
            &temp.path().join("events.jsonl"),
            &(1..=5)
                .map(|i| event(session, Some(i), "input"))
                .collect::<Vec<_>>(),
        );
        let mut records = timeline();
        records.push(json!({"type":"checkpoint_created","id":"C","parent":"A","prompt":"interrupted", "event_start":5,"snapshot":null,"snapshot_error":"snapshot unavailable","created_at":"2026-10-19T00:00:00Z"}));
        write_records(&temp.path().join("timeline.jsonl"), &records);
        write_records(
            &temp.path().join("compaction-checkpoints.jsonl"),
            &[json!({"record_type":"proactive_attempt","record":{
                "session_id":session,"provider_type":"openai","model":"test","source_sequence":5,
                "estimated_input_tokens":9000,"input_message_count":5,"source_fingerprint":vec![3;32],
            }})],
        );
        let source = LegacySessionSource::read(temp.path(), session).unwrap();
        assert_eq!(source.conversation.pending.as_deref(), Some("C"));
        assert_eq!(source.conversation.active_ranges, [(1, 2), (5, 5)]);
        let attempt = &source.compaction_attempts[0];
        assert_eq!(
            (&*attempt.provider_type, &*attempt.model),
            ("openai", "test")
        );
        assert_eq!(attempt.attempt.source_sequence, 5);
        assert_eq!(attempt.attempt.estimated_input_tokens, 9000);
        assert_eq!(attempt.attempt.source_fingerprint, [3; 32]);
    }

    #[test]
    fn refuses_damaged_timeline_compaction_and_workspace_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        journal(
            &temp.path().join("events.jsonl"),
            &[event(session, Some(1), "input")],
        );
        for name in [
            "timeline.jsonl",
            "compaction-checkpoints.jsonl",
            "workspace.json",
        ] {
            let path = temp.path().join(name);
            std::fs::write(&path, b"{\"incomplete\":").unwrap();
            assert!(
                LegacySessionSource::read(temp.path(), session).is_err(),
                "{name} must not be skipped"
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"{\"incomplete\":");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn bounds_source_record_allocation_and_accepts_the_exact_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("records.jsonl");
        let mut bytes = vec![b' '; MAX_RECORD_BYTES - 1];
        bytes.push(b'0');
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_records::<i32>(&path, &mut Sha256::new()).unwrap(), [0]);
        bytes.insert(0, b' ');
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_records::<i32>(&path, &mut Sha256::new()).is_err());
        let file = File::create(&path).unwrap();
        file.set_len(MAX_SOURCE_BYTES as u64 + 1).unwrap();
        drop(file);
        assert!(
            read_records::<i32>(&path, &mut Sha256::new())
                .unwrap_err()
                .to_string()
                .contains("recovery byte limit")
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_and_hard_linked_sources() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let original = temp.path().join("original");
        journal(&original, &[event(session, Some(1), "input")]);
        let path = temp.path().join("events.jsonl");
        symlink(&original, &path).unwrap();
        assert!(LegacySessionSource::read(temp.path(), session).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&original, &path).unwrap();
        assert!(LegacySessionSource::read(temp.path(), session).is_err());
    }
}

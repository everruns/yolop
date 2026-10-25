//! Read Yolop's saved format before handing ownership to an Engine authority.
//!
//! This reader never starts a legacy runtime or changes the source. Import
//! needs the complete journal, including abandoned branches, rather than the
//! tolerant, active-branch projection used to render an older session.

use anyhow::{Context, Result, bail};
use everruns_contracts::typed_id::SessionId;
use everruns_core::{CompactionCheckpoint, Event};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
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
    _lease: SessionFileLease,
}

impl LegacySessionSource {
    /// Retain the same exclusive lease used by Yolop execution until this
    /// source is dropped. Malformed or foreign records refuse migration;
    /// replay's best-effort skipping is unsuitable for an ownership transfer.
    pub fn read(session_dir: &Path, session_id: SessionId) -> Result<Self> {
        if !session_dir.is_dir() {
            bail!("migration source session does not exist");
        }
        let lease = lock_session(session_dir)?;
        let mut digest = Sha256::new();
        let mut events: Vec<Event> = read_records(&session_log_path(session_dir), &mut digest)?;
        let mut sequence = 0i32;
        let mut ids = HashSet::new();
        for event in &mut events {
            if event.session_id != session_id {
                bail!("migration journal contains an event for another session");
            }
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
            fingerprint: digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            _lease: lease,
        })
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
            "event_count":self.events.len(),
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

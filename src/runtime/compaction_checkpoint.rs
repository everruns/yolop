//! Owner-private, append-only persistence for provider compaction checkpoints.

use async_trait::async_trait;
use everruns_core::error::{AgentLoopError, Result};
use everruns_core::typed_id::SessionId;
use everruns_core::{
    CompactionCheckpoint, CompactionCheckpointPayload, CompactionCheckpointStore,
    ProactiveCompactionAttempt,
};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::session_state::checkpoint::CheckpointManager;

const CHECKPOINT_LOG: &str = "compaction-checkpoints.jsonl";

type ActiveSequenceFilter = dyn Fn(i64) -> bool + Send + Sync;

pub(crate) struct JsonlCompactionCheckpointStore {
    session_id: SessionId,
    path: PathBuf,
    active_sequence: Arc<ActiveSequenceFilter>,
    access: Mutex<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCheckpoint {
    id: String,
    session_id: SessionId,
    source_sequence: i64,
    provider_type: String,
    model: String,
    format_version: u32,
    payload: CompactionCheckpointPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAttempt {
    session_id: SessionId,
    provider_type: String,
    model: String,
    source_sequence: i64,
    estimated_input_tokens: u64,
    input_message_count: usize,
    source_fingerprint: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "record_type", content = "record", rename_all = "snake_case")]
enum StoredRecord {
    Checkpoint(StoredCheckpoint),
    ProactiveAttempt(StoredAttempt),
}

#[derive(Default)]
struct StoredState {
    checkpoints: Vec<CompactionCheckpoint>,
    attempts: Vec<StoredAttempt>,
}

impl JsonlCompactionCheckpointStore {
    pub(crate) fn open(
        session_dir: &Path,
        session_id: SessionId,
        timeline: Arc<CheckpointManager>,
    ) -> Result<Self> {
        let timeline_filter = timeline.clone();
        Self::with_active_sequence(
            session_dir.join(CHECKPOINT_LOG),
            session_id,
            Arc::new(move |sequence| timeline_filter.is_event_sequence_active(sequence)),
        )
    }

    fn with_active_sequence(
        path: PathBuf,
        session_id: SessionId,
        active_sequence: Arc<ActiveSequenceFilter>,
    ) -> Result<Self> {
        match open_private_read(&path) {
            Ok(file) => tighten_file_permissions(&file)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(store_error("open existing checkpoint log", error)),
        }
        Ok(Self {
            session_id,
            path,
            active_sequence,
            access: Mutex::new(()),
        })
    }

    fn load(&self) -> Result<StoredState> {
        let file = match open_private_read(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(StoredState::default());
            }
            Err(error) => return Err(store_error("open checkpoint log", error)),
        };
        let mut state = StoredState::default();
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|error| store_error("read checkpoint log", error))?;
            if line.trim().is_empty() {
                continue;
            }
            let stored: StoredRecord = match serde_json::from_str(&line) {
                Ok(stored) => stored,
                Err(error) => {
                    tracing::warn!(
                        line = index + 1,
                        error = %error,
                        "skipping malformed private compaction record"
                    );
                    continue;
                }
            };
            match stored {
                StoredRecord::Checkpoint(checkpoint) => {
                    if checkpoint.session_id != self.session_id {
                        tracing::warn!(
                            line = index + 1,
                            "skipping compaction checkpoint for another session"
                        );
                        continue;
                    }
                    state.checkpoints.push(checkpoint.try_into()?);
                }
                StoredRecord::ProactiveAttempt(attempt) => {
                    if attempt.session_id != self.session_id {
                        tracing::warn!(
                            line = index + 1,
                            "skipping compaction attempt for another session"
                        );
                        continue;
                    }
                    state.attempts.push(attempt);
                }
            }
        }
        Ok(state)
    }

    fn append(&self, record: &StoredRecord) -> Result<()> {
        let mut options = OpenOptions::new();
        options.create(true).append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|error| store_error("open checkpoint log for append", error))?;
        validate_private_file(&file)?;
        tighten_file_permissions(&file)?;
        let length = file
            .metadata()
            .map_err(|error| store_error("inspect checkpoint log", error))?
            .len();
        if length > 0 {
            file.seek(SeekFrom::End(-1))
                .map_err(|error| store_error("seek checkpoint log", error))?;
            let mut last = [0_u8; 1];
            file.read_exact(&mut last)
                .map_err(|error| store_error("read checkpoint log tail", error))?;
            if last[0] != b'\n' {
                // A process crash can leave a partial final JSON object. Keep it
                // isolated so the next valid append remains independently readable.
                file.write_all(b"\n")
                    .map_err(|error| store_error("repair checkpoint log tail", error))?;
            }
        }
        serde_json::to_writer(&mut file, record)
            .map_err(|error| store_error("serialize compaction record", error))?;
        file.write_all(b"\n")
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_data())
            .map_err(|error| store_error("persist checkpoint", error))
    }
}

#[async_trait]
impl CompactionCheckpointStore for JsonlCompactionCheckpointStore {
    async fn get_latest(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
    ) -> Result<Option<CompactionCheckpoint>> {
        if session_id != self.session_id {
            return Ok(None);
        }
        let _guard = self.access.lock().await;
        Ok(self
            .load()?
            .checkpoints
            .into_iter()
            .filter(|checkpoint| {
                checkpoint.provider_type == provider_type
                    && checkpoint.model == model
                    && (self.active_sequence)(checkpoint.source_sequence)
            })
            .max_by_key(|checkpoint| checkpoint.source_sequence))
    }

    async fn install(&self, checkpoint: CompactionCheckpoint) -> Result<bool> {
        if checkpoint.session_id != self.session_id
            || !(self.active_sequence)(checkpoint.source_sequence)
        {
            return Ok(false);
        }
        let _guard = self.access.lock().await;
        let newer_exists = self.load()?.checkpoints.into_iter().any(|current| {
            current.provider_type == checkpoint.provider_type
                && current.model == checkpoint.model
                && current.format_version == checkpoint.format_version
                && (self.active_sequence)(current.source_sequence)
                && current.source_sequence >= checkpoint.source_sequence
        });
        if newer_exists {
            return Ok(false);
        }
        self.append(&StoredRecord::Checkpoint(StoredCheckpoint::from(
            &checkpoint,
        )))?;
        Ok(true)
    }

    async fn get_proactive_attempt(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
    ) -> Result<Option<ProactiveCompactionAttempt>> {
        if session_id != self.session_id {
            return Ok(None);
        }
        let _guard = self.access.lock().await;
        Ok(self
            .load()?
            .attempts
            .into_iter()
            .filter(|stored| {
                stored.provider_type == provider_type
                    && stored.model == model
                    && (self.active_sequence)(stored.source_sequence)
            })
            .max_by_key(|stored| stored.source_sequence)
            .map(StoredAttempt::into_attempt))
    }

    async fn record_proactive_attempt(
        &self,
        session_id: SessionId,
        provider_type: &str,
        model: &str,
        attempt: ProactiveCompactionAttempt,
    ) -> Result<()> {
        if session_id != self.session_id || !(self.active_sequence)(attempt.source_sequence) {
            return Ok(());
        }
        let _guard = self.access.lock().await;
        let duplicate_or_newer = self.load()?.attempts.into_iter().any(|stored| {
            stored.provider_type == provider_type
                && stored.model == model
                && (self.active_sequence)(stored.source_sequence)
                && stored.source_sequence >= attempt.source_sequence
        });
        if !duplicate_or_newer {
            self.append(&StoredRecord::ProactiveAttempt(StoredAttempt {
                session_id,
                provider_type: provider_type.to_string(),
                model: model.to_string(),
                source_sequence: attempt.source_sequence,
                estimated_input_tokens: attempt.estimated_input_tokens,
                input_message_count: attempt.input_message_count,
                source_fingerprint: attempt.source_fingerprint,
            }))?;
        }
        Ok(())
    }
}

impl StoredAttempt {
    fn into_attempt(self) -> ProactiveCompactionAttempt {
        ProactiveCompactionAttempt {
            source_sequence: self.source_sequence,
            estimated_input_tokens: self.estimated_input_tokens,
            input_message_count: self.input_message_count,
            source_fingerprint: self.source_fingerprint,
        }
    }
}

impl From<&CompactionCheckpoint> for StoredCheckpoint {
    fn from(checkpoint: &CompactionCheckpoint) -> Self {
        Self {
            id: checkpoint.id.to_string(),
            session_id: checkpoint.session_id,
            source_sequence: checkpoint.source_sequence,
            provider_type: checkpoint.provider_type.clone(),
            model: checkpoint.model.clone(),
            format_version: checkpoint.format_version,
            payload: checkpoint.payload.clone(),
        }
    }
}

impl TryFrom<StoredCheckpoint> for CompactionCheckpoint {
    type Error = AgentLoopError;

    fn try_from(stored: StoredCheckpoint) -> Result<Self> {
        Ok(Self {
            id: stored
                .id
                .parse()
                .map_err(|error| store_error("parse checkpoint id", error))?,
            session_id: stored.session_id,
            source_sequence: stored.source_sequence,
            provider_type: stored.provider_type,
            model: stored.model,
            format_version: stored.format_version,
            payload: stored.payload,
        })
    }
}

fn open_private_read(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    validate_private_file_io(&file)?;
    Ok(file)
}

fn validate_private_file(file: &File) -> Result<()> {
    validate_private_file_io(file).map_err(|error| store_error("validate checkpoint log", error))
}

fn validate_private_file_io(file: &File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "checkpoint log is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "checkpoint log has multiple hard links",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn tighten_file_permissions(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| store_error("set checkpoint permissions", error))
}

#[cfg(not(unix))]
fn tighten_file_permissions(_file: &File) -> Result<()> {
    Ok(())
}

fn store_error(action: &str, error: impl std::fmt::Display) -> AgentLoopError {
    AgentLoopError::store(format!("{action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::{
        COMPACTION_CHECKPOINT_FORMAT_VERSION, CompactOutputItem, ProviderOpaqueContext,
    };
    use std::collections::BTreeSet;
    use std::sync::RwLock;

    fn checkpoint(session_id: SessionId, sequence: i64) -> CompactionCheckpoint {
        CompactionCheckpoint {
            id: format!("00000000-0000-7000-8000-{sequence:012}")
                .parse()
                .expect("checkpoint UUID"),
            session_id,
            source_sequence: sequence,
            provider_type: "openai-codex".to_string(),
            model: "gpt-5.6".to_string(),
            format_version: COMPACTION_CHECKPOINT_FORMAT_VERSION,
            payload: CompactionCheckpointPayload::ProviderOpaque {
                context: ProviderOpaqueContext::OpenResponsesCompact {
                    output: vec![CompactOutputItem::Compaction {
                        encrypted_content: format!("opaque-{sequence}"),
                    }],
                },
            },
        }
    }

    fn attempt(sequence: i64) -> ProactiveCompactionAttempt {
        ProactiveCompactionAttempt {
            source_sequence: sequence,
            estimated_input_tokens: sequence as u64 * 1_000,
            input_message_count: sequence as usize,
            source_fingerprint: [sequence as u8; 32],
        }
    }

    #[tokio::test]
    async fn persists_latest_active_checkpoint_across_store_instances() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CHECKPOINT_LOG);
        let session_id = SessionId::new();
        let active = Arc::new(RwLock::new(BTreeSet::<i64>::from([10, 20])));
        let filter = {
            let active = active.clone();
            Arc::new(move |sequence| active.read().unwrap().contains(&sequence))
                as Arc<ActiveSequenceFilter>
        };
        let first = JsonlCompactionCheckpointStore::with_active_sequence(
            path.clone(),
            session_id,
            filter.clone(),
        )
        .unwrap();
        assert!(first.install(checkpoint(session_id, 10)).await.unwrap());
        assert!(first.install(checkpoint(session_id, 20)).await.unwrap());
        assert!(!first.install(checkpoint(session_id, 20)).await.unwrap());

        let resumed =
            JsonlCompactionCheckpointStore::with_active_sequence(path, session_id, filter.clone())
                .unwrap();
        assert_eq!(
            resumed
                .get_latest(session_id, "openai-codex", "gpt-5.6")
                .await
                .unwrap()
                .unwrap()
                .source_sequence,
            20
        );

        active.write().unwrap().remove(&20);
        assert_eq!(
            resumed
                .get_latest(session_id, "openai-codex", "gpt-5.6")
                .await
                .unwrap()
                .unwrap()
                .source_sequence,
            10,
            "an abandoned checkpoint must not shadow the surviving branch"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn checkpoint_log_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CHECKPOINT_LOG);
        let session_id = SessionId::new();
        let store = JsonlCompactionCheckpointStore::with_active_sequence(
            path.clone(),
            session_id,
            Arc::new(|_| true),
        )
        .unwrap();
        assert!(store.install(checkpoint(session_id, 1)).await.unwrap());
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn persists_attempt_watermark_and_tracks_the_active_branch() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CHECKPOINT_LOG);
        let session_id = SessionId::new();
        let active = Arc::new(RwLock::new(BTreeSet::<i64>::from([10, 20])));
        let filter = {
            let active = active.clone();
            Arc::new(move |sequence| active.read().unwrap().contains(&sequence))
                as Arc<ActiveSequenceFilter>
        };
        let first = JsonlCompactionCheckpointStore::with_active_sequence(
            path.clone(),
            session_id,
            filter.clone(),
        )
        .unwrap();
        first
            .record_proactive_attempt(session_id, "openai-codex", "gpt-5.6", attempt(10))
            .await
            .unwrap();
        first
            .record_proactive_attempt(session_id, "openai-codex", "gpt-5.6", attempt(20))
            .await
            .unwrap();

        let resumed =
            JsonlCompactionCheckpointStore::with_active_sequence(path, session_id, filter).unwrap();
        assert_eq!(
            resumed
                .get_proactive_attempt(session_id, "openai-codex", "gpt-5.6")
                .await
                .unwrap()
                .unwrap(),
            attempt(20)
        );

        active.write().unwrap().remove(&20);
        assert_eq!(
            resumed
                .get_proactive_attempt(session_id, "openai-codex", "gpt-5.6")
                .await
                .unwrap()
                .unwrap(),
            attempt(10)
        );
    }

    #[tokio::test]
    async fn partial_crash_tail_does_not_hide_later_checkpoints() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(CHECKPOINT_LOG);
        std::fs::write(&path, b"{\"partial\":").unwrap();
        let session_id = SessionId::new();
        let store = JsonlCompactionCheckpointStore::with_active_sequence(
            path,
            session_id,
            Arc::new(|_| true),
        )
        .unwrap();

        assert!(store.install(checkpoint(session_id, 1)).await.unwrap());
        assert_eq!(
            store
                .get_latest(session_id, "openai-codex", "gpt-5.6")
                .await
                .unwrap()
                .unwrap()
                .source_sequence,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_checkpoint_log_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, "do not touch").unwrap();
        let path = temp.path().join(CHECKPOINT_LOG);
        symlink(&target, &path).unwrap();

        let error = JsonlCompactionCheckpointStore::with_active_sequence(
            path,
            SessionId::new(),
            Arc::new(|_| true),
        )
        .err()
        .expect("symlink must be rejected");
        assert!(error.to_string().contains("checkpoint log"));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "do not touch");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_checkpoint_log_hard_links() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        std::fs::write(&target, "do not touch").unwrap();
        let path = temp.path().join(CHECKPOINT_LOG);
        std::fs::hard_link(&target, &path).unwrap();

        assert!(
            JsonlCompactionCheckpointStore::with_active_sequence(
                path,
                SessionId::new(),
                Arc::new(|_| true),
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "do not touch");
    }
}

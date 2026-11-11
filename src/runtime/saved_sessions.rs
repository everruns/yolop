//! Read retained sessions from their authority, with legacy journal import support.
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use everruns::hosting::SqliteHosting;
use everruns_contracts::typed_id::SessionId;
use everruns_core::Event;
use serde_json::Value;

fn databases(directory: &Path, session: SessionId) -> [PathBuf; 3] {
    let root = directory.parent().unwrap_or(directory);
    [
        directory.join("engine.sqlite"),
        root.join(".engine")
            .join(session.to_string())
            .join("engine.sqlite"),
        root.join("engine.sqlite"),
    ]
}

pub(crate) fn modified(directory: &Path, session: SessionId) -> std::time::SystemTime {
    let files = match databases(directory, session)
        .into_iter()
        .take(2)
        .find(|path| path.is_file())
    {
        // A committed WAL write need not touch the session directory or main DB.
        Some(database) => vec![database.with_extension("sqlite-wal"), database],
        None => vec![legacy_journal(directory, session)],
    };
    files
        .into_iter()
        .filter_map(|path| path.metadata().ok()?.modified().ok())
        .max()
        .or_else(|| directory.metadata().ok()?.modified().ok())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

pub(super) fn visit_native(
    directory: &Path,
    session: SessionId,
    mut visit: impl FnMut(Event) -> everruns_contracts::error::Result<()>,
) -> Result<Option<PathBuf>> {
    for database in databases(directory, session) {
        if database.exists()
            && SqliteHosting::visit_session_events(&database, session, &mut visit)
                .with_context(|| format!("read session database {}", database.display()))?
        {
            return Ok(Some(database));
        }
    }
    Ok(None)
}

pub(crate) fn legacy_journal(directory: &Path, session: SessionId) -> PathBuf {
    let path = directory.join("events.jsonl");
    if path.exists() {
        path
    } else {
        super::session_log::legacy_session_log_path(
            directory.parent().unwrap_or(directory),
            session,
        )
    }
}

pub(crate) async fn exists(directory: &Path, session: SessionId) -> bool {
    let paths = databases(directory, session);
    if paths[..2].iter().any(|path| path.exists()) || legacy_journal(directory, session).exists() {
        return true;
    }
    if !paths[2].exists() {
        return false;
    }
    // Shared-only legacy sessions need a content probe. Keep SQLite waits and
    // journal decoding off ACP's async thread; arbitrary IDs must stay rejected.
    tokio::task::spawn_blocking(move || {
        SqliteHosting::visit_session_events(&paths[2], session, |_| Ok(())).unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

pub(crate) fn visit_records(
    directory: &Path,
    session: SessionId,
    mut visit: impl FnMut(Value),
) -> Result<Option<PathBuf>> {
    if let Some(path) = visit_native(directory, session, |event| {
        let value = serde_json::to_value(event)
            .map_err(|failure| everruns_contracts::AgentLoopError::store(failure.to_string()))?;
        visit(value);
        Ok(())
    })? {
        return Ok(Some(path));
    }
    let path = legacy_journal(directory, session);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read legacy journal {}", path.display()));
        }
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Ok(record) = serde_json::from_str(&line) {
            visit(record);
        }
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shared_existence_probe_does_not_block_the_async_thread() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let shared = root.path().join("engine.sqlite");
        drop(SqliteHosting::open(&shared).unwrap());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let database = rusqlite::Connection::open(shared).unwrap();
            database
                .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
                .unwrap();
            ready_tx.send(()).unwrap();
            let released = release_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            database.execute_batch("ROLLBACK").unwrap();
            released
        });
        ready_rx.recv().unwrap();
        let directory = root.path().join(session.to_string());
        let probe = exists(&directory, session);
        tokio::pin!(probe);
        // Poll once while the reader is locked, then release from this same
        // async thread. A synchronous SQLite wait prevents that release.
        let first_poll = futures::poll!(probe.as_mut());
        let released = release_tx.send(());
        assert!(writer.join().unwrap(), "probe blocked the async thread");
        released.unwrap();
        let found = match first_poll {
            std::task::Poll::Ready(found) => found,
            std::task::Poll::Pending => probe.await,
        };
        assert!(!found, "unknown shared-only IDs must stay rejected");
    }

    #[test]
    fn a_shared_database_write_does_not_change_session_recency() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let directory = root.path().join(session.to_string());
        std::fs::create_dir(&directory).unwrap();
        let journal = directory.join("events.jsonl");
        std::fs::write(&journal, "{}").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(journal)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        std::fs::write(root.path().join("engine.sqlite"), "another session's write").unwrap();
        assert_eq!(
            modified(&directory, session),
            std::time::SystemTime::UNIX_EPOCH
        );
    }

    #[test]
    fn legacy_sources_are_read_in_place_with_folder_precedence() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let directory = root.path().join(session.to_string());
        let flat = root.path().join(format!("{session}.jsonl"));
        std::fs::write(&flat, "{\"text\":\"flat\"}\n").unwrap();
        let mut records = Vec::new();
        assert_eq!(
            visit_records(&directory, session, |value| records.push(value)).unwrap(),
            Some(flat.clone())
        );
        assert_eq!(records[0]["text"], "flat");
        assert!(!directory.exists());
        std::fs::create_dir(&directory).unwrap();
        let current = directory.join("events.jsonl");
        std::fs::write(&current, "{\"text\":\"folder\"}\n").unwrap();
        records.clear();
        assert_eq!(
            visit_records(&directory, session, |value| records.push(value)).unwrap(),
            Some(current)
        );
        assert_eq!(records[0]["text"], "folder");
        assert_eq!(
            std::fs::read_to_string(flat).unwrap(),
            "{\"text\":\"flat\"}\n"
        );
    }
}

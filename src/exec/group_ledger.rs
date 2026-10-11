//! A record of the command process groups a headless thread host started, so
//! they can be stopped when that host dies without stopping them itself.
//!
//! Every bash command runs in its own process group, which the bash tool kills
//! when the command ends, times out, or is dropped. A host killed outright
//! (`kill -9`, OOM, a reboot of the terminal it was not attached to) runs none
//! of that, and its builds and dev servers keep running unsupervised while the
//! coordinator restarts the thread on the same workspace.
//!
//! Decision: only headless `--serve` hosts keep a ledger. A person's TUI that
//! crashes leaves its processes alone, as it always has; a thread host is
//! restarted automatically, so leftovers would otherwise race its successor.
//! Decision: the ledger is keyed by host process, not session, and each entry
//! carries its process's start time. A pid or group id the kernel reused for
//! an unrelated process never matches, so reaping cannot signal a stranger.
//! Decision: a group whose leader already exited is not signalled, because its
//! id can no longer be verified. Background work started with `&` inside a
//! command that then returned is the gap this leaves.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The directory, under the sessions directory, holding one ledger file per
/// live or dead headless host.
pub(crate) const LEDGER_DIR: &str = ".thread-procs";

static LEDGER: OnceLock<Ledger> = OnceLock::new();

struct Ledger {
    path: PathBuf,
    groups: Mutex<Vec<(i32, u64)>>,
}

/// Start recording this process's command groups under `sessions_dir`. Called
/// once by a headless host before it runs anything.
pub(crate) fn enable(sessions_dir: &Path) -> anyhow::Result<()> {
    let dir = sessions_dir.join(LEDGER_DIR);
    fs::create_dir_all(&dir)?;
    let pid = std::process::id() as i32;
    let started = start_time(pid).unwrap_or(0);
    let path = dir.join(format!("{pid}-{started}"));
    fs::write(&path, "")?;
    let _ = LEDGER.set(Ledger {
        path,
        groups: Mutex::new(Vec::new()),
    });
    Ok(())
}

/// Note a command group this process just started. A no-op unless enabled.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn record(group: i32) {
    let Some(ledger) = LEDGER.get() else {
        return;
    };
    let Some(started) = start_time(group) else {
        return;
    };
    let mut groups = ledger.groups.lock().unwrap_or_else(|e| e.into_inner());
    groups.push((group, started));
    ledger.write(&groups);
}

/// Forget a command group this process has stopped or seen exit.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn forget(group: i32) {
    let Some(ledger) = LEDGER.get() else {
        return;
    };
    let mut groups = ledger.groups.lock().unwrap_or_else(|e| e.into_inner());
    groups.retain(|(id, _)| *id != group);
    ledger.write(&groups);
}

impl Ledger {
    fn write(&self, groups: &[(i32, u64)]) {
        let body: String = groups
            .iter()
            .map(|(group, started)| format!("{group} {started}\n"))
            .collect();
        // Write then rename, so a reaper never reads half a ledger.
        let tmp = self.path.with_extension("tmp");
        let written = fs::File::create(&tmp)
            .and_then(|mut file| file.write_all(body.as_bytes()))
            .and_then(|()| fs::rename(&tmp, &self.path));
        if let Err(error) = written {
            tracing::warn!(%error, "could not update the command group ledger");
        }
    }
}

/// Stop the command groups of every headless host under `sessions_dir` that is
/// no longer running, and drop their ledgers. Returns how many groups were
/// signalled.
pub(crate) fn reap_dead_hosts(sessions_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(sessions_dir.join(LEDGER_DIR)) else {
        return 0;
    };
    let mut signalled = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some((pid, started)) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_host)
        else {
            continue;
        };
        if is_same_process(pid, started) {
            continue;
        }
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        for line in body.lines() {
            let mut fields = line.split_whitespace();
            let (Some(Ok(group)), Some(Ok(group_started))) = (
                fields.next().map(str::parse::<i32>),
                fields.next().map(str::parse::<u64>),
            ) else {
                continue;
            };
            if group > 1 && is_same_process(group, group_started) {
                kill_group(group);
                signalled += 1;
            }
        }
        let _ = fs::remove_file(&path);
    }
    signalled
}

fn parse_host(name: &str) -> Option<(i32, u64)> {
    let (pid, started) = name.split_once('-')?;
    Some((pid.parse().ok()?, started.parse().ok()?))
}

fn is_same_process(pid: i32, started: u64) -> bool {
    start_time(pid).is_some_and(|now| now == started)
}

#[cfg(unix)]
fn kill_group(group: i32) {
    // SAFETY: signals only the process group whose leader was just verified.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_group(_group: i32) {}

/// A process's start time, which with its pid identifies it uniquely. `None`
/// when the process does not exist.
#[cfg(target_os = "linux")]
fn start_time(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name in field 2 may contain spaces; fields after its closing
    // parenthesis are fixed. Start time is field 22, the 20th after it.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn start_time(pid: i32) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let output = super::proc::capture(std::process::Command::new("ps").args([
        "-o",
        "lstart=",
        "-p",
        &pid.to_string(),
    ]))
    .ok()?;
    let started = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || started.is_empty() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    started.hash(&mut hasher);
    Some(hasher.finish() >> 1)
}

#[cfg(not(unix))]
fn start_time(_pid: i32) -> Option<u64> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn write_ledger(dir: &Path, host: &str, groups: &[(i32, u64)]) {
        let dir = dir.join(LEDGER_DIR);
        fs::create_dir_all(&dir).unwrap();
        let body: String = groups.iter().map(|(g, s)| format!("{g} {s}\n")).collect();
        fs::write(dir.join(host), body).unwrap();
    }

    #[test]
    fn a_dead_hosts_command_groups_are_stopped_and_a_live_hosts_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let mut orphan = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = orphan.id() as i32;
        let started = start_time(group).unwrap();

        // A live host (this test process) keeps its groups running.
        let me = std::process::id() as i32;
        let host = format!("{me}-{}", start_time(me).unwrap());
        write_ledger(tmp.path(), &host, &[(group, started)]);
        assert_eq!(reap_dead_hosts(tmp.path()), 0);
        assert!(orphan.try_wait().unwrap().is_none());

        // A host whose pid now names another process counts as gone. A
        // reused group id (wrong start time) is still never signalled.
        let dead = format!("{me}-0");
        write_ledger(tmp.path(), &dead, &[(group, started + 1)]);
        assert_eq!(reap_dead_hosts(tmp.path()), 0);
        assert!(orphan.try_wait().unwrap().is_none());
        assert!(!tmp.path().join(LEDGER_DIR).join(&dead).exists());

        // A host that is gone has its groups stopped and its ledger dropped.
        write_ledger(tmp.path(), &dead, &[(group, started)]);
        assert_eq!(reap_dead_hosts(tmp.path()), 1);
        assert!(orphan.wait().unwrap().code().is_none());
        assert!(!tmp.path().join(LEDGER_DIR).join(&dead).exists());
    }
}

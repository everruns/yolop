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
//! Decision: a group whose leader already exited (a command that started
//! background work with `&` and returned) is still stopped when its remaining
//! members can be verified instead: no process holds the group id as its pid,
//! and each member is in the group, in the session the leader was in, and
//! started no earlier than the leader. The kernel never hands out a pid that
//! names a live process group, so such a group cannot have been reissued to a
//! stranger outside that terminal session. Linux only; elsewhere a group
//! without its leader is left alone.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The directory, under the sessions directory, holding one ledger file per
/// live or dead headless host.
pub(crate) const LEDGER_DIR: &str = ".thread-procs";

static LEDGER: OnceLock<Ledger> = OnceLock::new();

/// One recorded command group: its id, its leader's start time, and the
/// session the leader ran in (0 where that is not known).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Group {
    id: i32,
    started: u64,
    session: i32,
}

struct Ledger {
    path: PathBuf,
    groups: Mutex<Vec<Group>>,
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
    let session = session_of(group).unwrap_or(0);
    let mut groups = ledger.groups.lock().unwrap_or_else(|e| e.into_inner());
    // A command that finished normally keeps its entry, since it may have left
    // a service running; drop the ones with nothing left to stop.
    groups.retain(still_running);
    groups.push(Group {
        id: group,
        started,
        session,
    });
    ledger.write(&groups);
}

/// Forget a command group this process has stopped or seen exit.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn forget(group: i32) {
    let Some(ledger) = LEDGER.get() else {
        return;
    };
    let mut groups = ledger.groups.lock().unwrap_or_else(|e| e.into_inner());
    groups.retain(|recorded| recorded.id != group);
    ledger.write(&groups);
}

impl Ledger {
    fn write(&self, groups: &[Group]) {
        let body: String = groups.iter().map(Group::line).collect();
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
        for group in body.lines().filter_map(Group::parse) {
            if still_running(&group) {
                kill_group(group.id);
                signalled += 1;
            }
        }
        let _ = fs::remove_file(&path);
    }
    signalled
}

impl Group {
    fn line(&self) -> String {
        format!("{} {} {}\n", self.id, self.started, self.session)
    }

    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.split_whitespace();
        let id = fields.next()?.parse().ok()?;
        let started = fields.next()?.parse().ok()?;
        let session = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        Some(Self {
            id,
            started,
            session,
        })
    }
}

/// Whether `group` is still the group this host started and has a process in
/// it: its leader, or verified members once the leader has exited.
fn still_running(group: &Group) -> bool {
    if group.id <= 1 {
        return false;
    }
    if start_time(group.id).is_some() {
        // A process holds the id: the leader, or a stranger the id was reused for.
        return is_same_process(group.id, group.started);
    }
    group.session > 0 && has_members(group)
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
    proc_stat(pid).map(|stat| stat.started)
}

#[cfg(target_os = "linux")]
fn session_of(pid: i32) -> Option<i32> {
    proc_stat(pid).map(|stat| stat.session)
}

#[cfg(target_os = "linux")]
struct ProcStat {
    group: i32,
    session: i32,
    started: u64,
}

#[cfg(target_os = "linux")]
fn proc_stat(pid: i32) -> Option<ProcStat> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name in field 2 may contain spaces; fields after its closing
    // parenthesis are fixed: process group is field 5, session field 6, and
    // start time field 22.
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    Some(ProcStat {
        group: fields.get(2)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        started: fields.get(19)?.parse().ok()?,
    })
}

/// Whether some process is still in `group`, in the session its leader ran in,
/// and started no earlier than the leader.
#[cfg(target_os = "linux")]
fn has_members(group: &Group) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(proc_stat)
        .any(|stat| {
            stat.group == group.id && stat.session == group.session && stat.started >= group.started
        })
}

#[cfg(not(target_os = "linux"))]
fn session_of(_pid: i32) -> Option<i32> {
    None
}

#[cfg(not(target_os = "linux"))]
fn has_members(_group: &Group) -> bool {
    false
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
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    fn write_ledger(dir: &Path, host: &str, groups: &[Group]) {
        let dir = dir.join(LEDGER_DIR);
        fs::create_dir_all(&dir).unwrap();
        let body: String = groups.iter().map(Group::line).collect();
        fs::write(dir.join(host), body).unwrap();
    }

    fn recorded(id: i32) -> Group {
        Group {
            id,
            started: start_time(id).unwrap(),
            session: session_of(id).unwrap(),
        }
    }

    fn gone_or_zombie(pid: i32) -> bool {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat[stat.rfind(')').unwrap() + 1..]
                .trim_start()
                .starts_with('Z'),
        }
    }

    #[test]
    fn a_dead_hosts_command_groups_are_stopped_and_a_live_hosts_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let mut orphan = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = recorded(orphan.id() as i32);

        // A live host (this test process) keeps its groups running.
        let me = std::process::id() as i32;
        let host = format!("{me}-{}", start_time(me).unwrap());
        write_ledger(tmp.path(), &host, &[group]);
        assert_eq!(reap_dead_hosts(tmp.path()), 0);
        assert!(orphan.try_wait().unwrap().is_none());

        // A host whose pid now names another process counts as gone. A
        // reused group id (wrong start time) is still never signalled.
        let dead = format!("{me}-0");
        let reused = Group {
            started: group.started + 1,
            ..group
        };
        write_ledger(tmp.path(), &dead, &[reused]);
        assert_eq!(reap_dead_hosts(tmp.path()), 0);
        assert!(orphan.try_wait().unwrap().is_none());
        assert!(!tmp.path().join(LEDGER_DIR).join(&dead).exists());

        // A host that is gone has its groups stopped and its ledger dropped.
        write_ledger(tmp.path(), &dead, &[group]);
        assert_eq!(reap_dead_hosts(tmp.path()), 1);
        assert!(orphan.wait().unwrap().code().is_none());
        assert!(!tmp.path().join(LEDGER_DIR).join(&dead).exists());
    }

    #[test]
    fn background_work_outliving_its_command_is_stopped_with_a_dead_host() {
        let tmp = tempfile::tempdir().unwrap();
        // The shell is the group leader; it starts `sleep` in the background,
        // reports its pid, and exits once its stdin closes, as a command that
        // ran `server &` does when it returns.
        let mut shell = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; read _"])
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let group = recorded(shell.id() as i32);
        let mut line = String::new();
        BufReader::new(shell.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let background: i32 = line.trim().parse().unwrap();
        drop(shell.stdin.take());
        shell.wait().unwrap();
        assert!(start_time(group.id).is_none(), "the leader has exited");
        assert!(!gone_or_zombie(background));

        let me = std::process::id() as i32;
        let dead = format!("{me}-0");

        // Members are only trusted in the leader's own terminal session.
        let elsewhere = Group {
            session: group.session + 1,
            ..group
        };
        write_ledger(tmp.path(), &dead, &[elsewhere]);
        assert_eq!(reap_dead_hosts(tmp.path()), 0);
        assert!(!gone_or_zombie(background));

        write_ledger(tmp.path(), &dead, &[group]);
        assert_eq!(reap_dead_hosts(tmp.path()), 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gone_or_zombie(background) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone_or_zombie(background), "the background job was stopped");
    }

    #[test]
    fn ledgers_from_before_sessions_were_recorded_still_parse() {
        assert_eq!(
            Group::parse("42 1000"),
            Some(Group {
                id: 42,
                started: 1000,
                session: 0
            })
        );
    }
}

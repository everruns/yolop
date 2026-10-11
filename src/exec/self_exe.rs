//! Re-executing this yolop binary after it may have been replaced on disk.
//!
//! A long-lived session re-executes itself for the sandbox worker, the
//! attached control child, and the `yolop` shim its shell sees. An upgrade
//! (`brew upgrade`, `cargo install`, a rebuild) replaces the file under the
//! running process; on Linux `current_exe()` then reports `<path> (deleted)`,
//! which cannot be executed, and every one of those paths broke until restart.
//! On Linux `/proc/<pid>/exe` still resolves to the running image even after
//! the file was replaced, so the session keeps re-executing the exact binary it
//! runs, which also keeps the control protocol on both ends identical.

use std::path::PathBuf;

/// Path that executes this process's own binary, valid for the process
/// lifetime even if the file on disk is replaced.
pub(crate) fn reexec_path() -> std::io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from(format!("/proc/{}/exe", std::process::id()));
        if path.exists() {
            return Ok(path);
        }
    }
    std::env::current_exe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexec_path_executes_this_binary() {
        let path = reexec_path().unwrap();
        let target = std::fs::canonicalize(&path).unwrap();
        assert_eq!(
            target,
            std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap()
        );
    }

    /// The failure this module exists for: copy a binary, start it, replace
    /// the file, and the `/proc/<pid>/exe` route still executes the original.
    #[cfg(target_os = "linux")]
    #[test]
    fn proc_exe_survives_replacing_the_binary() {
        let Ok(sleep) = which_sleep() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("sleeper");
        std::fs::copy(&sleep, &copy).unwrap();
        let mut child = std::process::Command::new(&copy).arg("30").spawn().unwrap();
        std::fs::remove_file(&copy).unwrap();
        std::fs::write(&copy, b"not a binary").unwrap();
        let proc_exe = PathBuf::from(format!("/proc/{}/exe", child.id()));
        let status = std::process::Command::new(&proc_exe)
            .arg("0")
            .status()
            .unwrap();
        child.kill().unwrap();
        let _ = child.wait();
        assert!(status.success());
    }

    #[cfg(target_os = "linux")]
    fn which_sleep() -> Result<PathBuf, ()> {
        ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.exists())
            .ok_or(())
    }
}

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

#[cfg(any(target_os = "macos", test))]
pub(super) fn admit_control_location(
    executable: &std::path::Path,
    write_roots: &[PathBuf],
    links: u64,
) -> anyhow::Result<()> {
    // TM-BASH: credentials authenticate descendants, not executable content.
    // A canonical pathname is trusted here only because none of this shell's
    // write grants can modify it or an ancestor. Multiple hard links would
    // leave another pathname outside that check.
    anyhow::ensure!(
        links == 1 && !write_roots.iter().any(|root| executable.starts_with(root)),
        "macOS attached control requires a single-link executable outside every sandbox write root; install or build Yolop in a protected location and restart"
    );
    Ok(())
}

/// Linux's running image survives replacement. Other platforms return a
/// mutable pathname, which needs admission before a contained client can
/// cause the host to execute it.
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

/// Capture the control parser's target before exposing an endpoint. macOS
/// retains the first canonical target for the process lifetime, including
/// shells without an endpoint, so a later command cannot choose a new alias.
pub(crate) fn control_reexec_path() -> std::io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        static TARGET: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        if let Some(path) = TARGET.get() {
            return Ok(path.clone());
        }
        let path = std::fs::canonicalize(reexec_path()?)?;
        Ok(TARGET.get_or_init(|| path).clone())
    }
    #[cfg(not(target_os = "macos"))]
    reexec_path()
}

#[cfg(any(target_os = "macos", all(test, unix)))]
pub(crate) fn admit_control_path(
    executable: &std::path::Path,
    write_roots: &[PathBuf],
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(executable)?;
    anyhow::ensure!(
        metadata.is_file(),
        "control parser target must be a regular file"
    );
    anyhow::ensure!(
        std::fs::canonicalize(executable)? == executable,
        "control parser target must be a captured canonical path"
    );
    let roots = write_roots
        .iter()
        .map(std::fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()?;
    admit_control_location(executable, &roots, metadata.nlink())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_image_admission_rejects_workspace_and_temp_locations() {
        let roots =
            ["/workspace", "/private/tmp", "/private/sandbox", "/skills"].map(PathBuf::from);
        for image in [
            "/workspace/target/debug/yolop",
            "/private/tmp/build/yolop",
            "/private/sandbox/yolop",
            "/skills/compiler/yolop",
        ] {
            assert!(admit_control_location(std::path::Path::new(image), &roots, 1).is_err());
        }
    }

    #[test]
    fn control_image_admission_preserves_protected_installations() {
        let roots = ["/workspace", "/private/tmp"].map(PathBuf::from);
        for image in [
            "/opt/homebrew/Cellar/yolop/bin/yolop",
            "/home/user/.cargo/bin/yolop",
            "/workspace-other/yolop",
        ] {
            assert!(admit_control_location(std::path::Path::new(image), &roots, 1).is_ok());
        }
        for links in [0, 2] {
            assert!(
                admit_control_location(std::path::Path::new("/opt/bin/yolop"), &roots, links)
                    .is_err()
            );
        }
        assert!(
            admit_control_location(
                std::path::Path::new("/opt/bin/yolop"),
                &[PathBuf::from("/")],
                1
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn control_image_admission_checks_ordinary_file_metadata() {
        // Regular data files only. Nothing is executable or launched.
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let trusted = root.join("trusted");
        let workspace = root.join("workspace");
        std::fs::create_dir(&trusted).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        let image = trusted.join("image.data");
        std::fs::write(&image, b"ordinary data").unwrap();
        let roots = [workspace.clone()];
        assert!(admit_control_path(&image, &roots).is_ok());
        assert!(admit_control_path(&trusted, &roots).is_err());
        assert!(admit_control_path(&image, &[root.join("missing")]).is_err());
        assert!(admit_control_path(&trusted.join("../trusted/image.data"), &roots).is_err());

        let linked = trusted.join("linked.data");
        std::fs::hard_link(&image, &linked).unwrap();
        assert!(admit_control_path(&image, &roots).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn control_image_admission_resolves_ordinary_path_aliases() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let trusted = root.join("trusted");
        std::fs::create_dir(&trusted).unwrap();
        let image = trusted.join("image.data");
        std::fs::write(&image, b"ordinary data").unwrap();
        let directory_alias = root.join("directory-alias");
        std::os::unix::fs::symlink(&trusted, &directory_alias).unwrap();
        assert!(admit_control_path(&image, &[directory_alias]).is_err());
        let file_alias = root.join("file-alias");
        std::os::unix::fs::symlink(&image, &file_alias).unwrap();
        assert!(admit_control_path(&file_alias, &[]).is_err());
    }

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
        let mut child = spawn_retrying_text_busy(&copy, "30");
        std::fs::remove_file(&copy).unwrap();
        std::fs::write(&copy, b"not a binary").unwrap();
        let proc_exe = PathBuf::from(format!("/proc/{}/exe", child.id()));
        let mut reexec = spawn_retrying_text_busy(&proc_exe, "0");
        let out = reexec.wait_with_output().unwrap();
        child.kill().unwrap();
        let _ = child.wait();
        assert!(
            out.status.success(),
            "{:?} stderr={} sleep={}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
            sleep.display()
        );
    }

    /// Another test thread forking while the copy's write fd is still open
    /// makes exec fail with ETXTBSY; the window closes once that child execs.
    #[cfg(target_os = "linux")]
    fn spawn_retrying_text_busy(program: &std::path::Path, arg: &str) -> std::process::Child {
        for _ in 0..50 {
            match std::process::Command::new(program)
                .arg(arg)
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => return other.unwrap(),
            }
        }
        panic!("{} stayed busy", program.display());
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

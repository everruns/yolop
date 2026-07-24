use rand::RngExt;
use std::backtrace::Backtrace;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_CRASH_REPORTS: usize = 5;
const MAX_PANIC_MESSAGE_BYTES: usize = 8 * 1024;
const MAX_REPORT_BYTES: usize = 256 * 1024;

pub(crate) struct CrashReporter {
    path: Option<PathBuf>,
    written: Arc<AtomicBool>,
}

impl CrashReporter {
    pub(crate) fn install() -> Self {
        let written = Arc::new(AtomicBool::new(false));
        let path = crash_report_dir()
            .and_then(|dir| prepare_crash_dir(&dir).then_some(dir))
            .map(|dir| {
                let timestamp = chrono::Utc::now().format("%Y-%m-%dT%H-%M-%SZ");
                let suffix = rand::rng().random::<u32>();
                dir.join(format!("{timestamp}-{suffix:08x}-crash.log"))
            });

        if let Some(path) = path.clone() {
            let hook_written = Arc::clone(&written);
            let crash_dir = path.parent().map(Path::to_path_buf);
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if hook_written
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    if write_panic_report(&path, info).is_err() {
                        hook_written.store(false, Ordering::SeqCst);
                    } else if let Some(dir) = crash_dir.as_deref() {
                        prune_old_reports(dir, MAX_CRASH_REPORTS);
                    }
                }
                previous(info);
            }));
        }

        Self { path, written }
    }

    pub(crate) fn report_path(&self) -> Option<&Path> {
        self.written
            .load(Ordering::SeqCst)
            .then_some(self.path.as_deref())
            .flatten()
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            path: None,
            written: Arc::new(AtomicBool::new(false)),
        }
    }
}

fn crash_report_dir() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("YOLOP_TEST_CRASH_DIR") {
        return Some(PathBuf::from(path));
    }
    dirs::data_dir().map(|dir| dir.join("yolop").join("crashes"))
}

fn prepare_crash_dir(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err() {
            return false;
        }
    }
    prune_old_reports(dir, MAX_CRASH_REPORTS);
    true
}

fn write_panic_report(path: &Path, info: &std::panic::PanicHookInfo<'_>) -> std::io::Result<()> {
    let mut payload = if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    };
    truncate_utf8(&mut payload, MAX_PANIC_MESSAGE_BYTES);
    let location = info
        .location()
        .map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        })
        .unwrap_or_else(|| "unknown".to_string());
    let thread = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_string();
    let mut body = format!(
        "timestamp: {}\nversion: {}\nthread: {thread}\nlocation: {location}\npanic: {payload}\n\nbacktrace:\n{}\n",
        chrono::Utc::now().to_rfc3339(),
        crate::version::VERSION_DETAILS,
        Backtrace::force_capture(),
    );
    truncate_utf8(&mut body, MAX_REPORT_BYTES);
    write_report(path, body.as_bytes())
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let boundary = value
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= max_bytes.saturating_sub('…'.len_utf8()))
        .last()
        .unwrap_or(0);
    value.truncate(boundary);
    value.push('…');
}

fn write_report(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(body)?;
    file.flush()
}

fn prune_old_reports(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut reports: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("-crash.log"))
        })
        .collect();
    reports.sort();
    let remove_count = reports.len().saturating_sub(keep);
    for path in reports.into_iter().take(remove_count) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn crash_report_is_owner_only() {
        let dir = tempfile::tempdir().expect("crash dir");
        let path = dir.path().join("report-crash.log");
        write_report(&path, b"panic: test\n").expect("write crash report");

        assert_eq!(std::fs::read(&path).expect("read report"), b"panic: test\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path)
                    .expect("report metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn crash_report_retention_keeps_newest_names() {
        let dir = tempfile::tempdir().expect("crash dir");
        for index in 0..7 {
            let path = dir
                .path()
                .join(format!("2026-07-24T06-44-{index:02}Z-deadbeef-crash.log"));
            write_report(&path, b"report").expect("write report");
        }

        prune_old_reports(dir.path(), 5);

        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read crash dir")
            .map(|entry| {
                entry
                    .expect("crash entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names.len(), 5);
        assert!(names[0].contains("06-44-02Z"));
        assert!(names[4].contains("06-44-06Z"));
    }

    #[test]
    fn crash_report_bounds_unicode_panic_text() {
        let mut message = "а".repeat(MAX_PANIC_MESSAGE_BYTES);
        truncate_utf8(&mut message, MAX_PANIC_MESSAGE_BYTES);

        assert!(message.len() <= MAX_PANIC_MESSAGE_BYTES);
        assert!(message.ends_with('…'));
    }

    #[test]
    fn panic_hook_writes_a_crash_report() {
        let dir = tempfile::tempdir().expect("crash dir");
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "crash_report::tests::panic_hook_child",
                "--nocapture",
            ])
            .env("YOLOP_TEST_CRASH_DIR", dir.path())
            .output()
            .expect("run panic child");

        assert!(!output.status.success(), "panic child should fail");
        let reports: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .expect("read crash dir")
            .map(|entry| entry.expect("crash entry").path())
            .collect();
        assert_eq!(reports.len(), 1);
        let report = std::fs::read_to_string(&reports[0]).expect("read crash report");
        assert!(report.contains("panic: crash-report-test"));
        assert!(report.contains("thread: crash_report::tests::panic_hook_child"));
        assert!(report.contains("backtrace:"));
    }

    #[test]
    fn panic_hook_child() {
        if std::env::var_os("YOLOP_TEST_CRASH_DIR").is_none() {
            return;
        }
        let _reporter = CrashReporter::install();
        panic!("crash-report-test");
    }
}

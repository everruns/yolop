//! Fetching weights into the model store, with visible progress.
//!
//! The engine can download its own weights, but it does so silently inside the
//! first inference call: a turn appears to hang for several gigabytes with
//! nothing on screen. Yolop pulls the files itself instead, so the wait is an
//! explicit `yolop weights pull` with a progress bar, and a turn either has its
//! weights or fails immediately with an actionable message.

use super::{GGUF_SEPARATOR, repo_dir, split_spec};
use anyhow::{Context, Result, anyhow};
use hf_hub::HFClient;
use hf_hub::progress::{DownloadEvent, ProgressEvent, ProgressHandler};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Files a safetensors repo needs beyond the weights themselves. The tokenizer
/// and chat template are what turn a tensor file into something that can hold a
/// tool-calling conversation, so a repo missing them is not usable even though
/// its weights downloaded fine.
const SAFETENSORS_SUPPORT_FILES: &[&str] = &[
    "config.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "generation_config.json",
    "special_tokens_map.json",
];

/// Support files whose absence is fine — not every repo ships all of them.
fn is_optional(file: &str) -> bool {
    matches!(
        file,
        "generation_config.json" | "special_tokens_map.json" | "tokenizer.json"
    )
}

/// Progress sink for a download. `pull` reports through this so tests can
/// observe byte accounting without a terminal, and the CLI can render a bar.
pub trait ProgressSink: Send {
    fn file_started(&mut self, filename: &str, total_bytes: Option<u64>);
    fn bytes_advanced(&mut self, bytes: u64);
    fn file_finished(&mut self);
}

/// Renders a single-line progress bar to stderr, refreshed at most ~20×/second
/// so a fast local link does not spend its time formatting.
pub struct StderrProgress {
    filename: String,
    total: Option<u64>,
    done: u64,
    last_draw: Option<Instant>,
}

impl Default for StderrProgress {
    fn default() -> Self {
        Self::new()
    }
}

impl StderrProgress {
    pub fn new() -> Self {
        Self {
            filename: String::new(),
            total: None,
            done: 0,
            last_draw: None,
        }
    }

    fn draw(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && self
                .last_draw
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(50))
        {
            return;
        }
        self.last_draw = Some(now);

        let mut err = std::io::stderr();
        match self.total {
            Some(total) if total > 0 => {
                let fraction = (self.done as f64 / total as f64).clamp(0.0, 1.0);
                let filled = (fraction * 30.0).round() as usize;
                let _ = write!(
                    err,
                    "\r  {:<28} [{}{}] {:>3}% {}",
                    truncate(&self.filename, 28),
                    "#".repeat(filled),
                    "-".repeat(30 - filled),
                    (fraction * 100.0).round() as u8,
                    super::human_bytes(total),
                );
            }
            // Hugging Face does not always report a length; show the running
            // total rather than a bar that cannot fill.
            _ => {
                let _ = write!(
                    err,
                    "\r  {:<28} {}",
                    truncate(&self.filename, 28),
                    super::human_bytes(self.done)
                );
            }
        }
        let _ = err.flush();
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let kept: String = value.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

impl ProgressSink for StderrProgress {
    fn file_started(&mut self, filename: &str, total_bytes: Option<u64>) {
        self.filename = filename.to_string();
        self.total = total_bytes;
        self.done = 0;
        self.draw(true);
    }

    fn bytes_advanced(&mut self, bytes: u64) {
        self.done = self.done.saturating_add(bytes);
        self.draw(false);
    }

    fn file_finished(&mut self) {
        self.draw(true);
        let _ = writeln!(std::io::stderr());
    }
}

/// One file per operation. Hub progress is cumulative and may report both
/// aggregate Xet counts and per-file updates, so only advance the high-water mark.
struct HubProgress {
    sink: Arc<Mutex<dyn ProgressSink>>,
    filename: String,
    completed: Mutex<u64>,
}

impl HubProgress {
    fn advance_to(&self, bytes: u64) {
        let Ok(mut completed) = self.completed.lock() else {
            return;
        };
        let delta = bytes.saturating_sub(*completed);
        *completed = (*completed).max(bytes);
        if delta > 0
            && let Ok(mut sink) = self.sink.lock()
        {
            sink.bytes_advanced(delta);
        }
    }
}

impl ProgressHandler for HubProgress {
    fn on_progress(&self, event: &ProgressEvent) {
        match event {
            ProgressEvent::Download(DownloadEvent::Start { total_bytes, .. }) => {
                if let Ok(mut sink) = self.sink.lock() {
                    sink.file_started(&self.filename, (*total_bytes > 0).then_some(*total_bytes));
                }
            }
            ProgressEvent::Download(DownloadEvent::Progress { files }) => {
                for file in files.iter().filter(|file| file.filename == self.filename) {
                    self.advance_to(file.bytes_completed);
                }
            }
            ProgressEvent::Download(DownloadEvent::AggregateProgress {
                bytes_completed, ..
            }) => {
                self.advance_to(*bytes_completed);
            }
            _ => {}
        }
    }
}

/// What a pull actually fetched.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PullSummary {
    pub files: Vec<String>,
    pub bytes: u64,
}

/// Download `spec` into the model store.
///
/// A GGUF spec fetches the named file; a safetensors spec fetches every
/// `.safetensors` shard plus its support
/// files. Files already present are skipped, so an interrupted pull resumes at
/// file granularity.
pub async fn pull(spec: &str, sink: Arc<Mutex<dyn ProgressSink>>) -> Result<PullSummary> {
    let (repo, gguf_file) = split_spec(spec);
    let dest = repo_dir(repo)?;
    let api = HFClient::new().context("build the Hugging Face client")?;
    pull_into(repo, gguf_file, &dest, sink, &api).await
}

async fn pull_into(
    repo: &str,
    gguf_file: Option<&str>,
    dest: &Path,
    sink: Arc<Mutex<dyn ProgressSink>>,
    api: &HFClient,
) -> Result<PullSummary> {
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let (owner, name) = hf_hub::split_id(repo);
    let repo_api = api.model(owner, name);

    let wanted = match gguf_file {
        Some(file) => vec![(file.to_string(), false)],
        None => {
            let info = repo_api
                .info()
                .send()
                .await
                .with_context(|| format!("list files in {repo}"))?;
            let shards: Vec<(String, bool)> = info
                .siblings
                .unwrap_or_default()
                .into_iter()
                .map(|sibling| sibling.rfilename)
                .filter(|name| {
                    name.ends_with(".safetensors") || name.ends_with(".safetensors.index.json")
                })
                .map(|name| (name, false))
                .collect();
            if shards.is_empty() {
                return Err(anyhow!(
                    "{repo} has no .safetensors weights. If this is a GGUF repo, name the file: \
                     {repo}{GGUF_SEPARATOR}<file>.gguf"
                ));
            }
            shards
                .into_iter()
                .chain(
                    SAFETENSORS_SUPPORT_FILES
                        .iter()
                        .map(|name| (name.to_string(), is_optional(name))),
                )
                .collect()
        }
    };

    let mut summary = PullSummary::default();
    for (filename, optional) in wanted {
        // `filename` is untrusted: it is either the `::` half of a CLI spec or
        // a name the remote repo listing supplied.
        let target = super::safe_join(dest, &filename)?;
        if target.is_file() {
            continue;
        }

        let progress = Arc::new(HubProgress {
            sink: sink.clone(),
            filename: filename.clone(),
            completed: Mutex::new(0),
        });
        // The new client writes directly into local_dir, including on failure.
        // Keep partial files private until success so a later pull never mistakes
        // an interrupted transfer for an installed weight file.
        let staging = tempfile::tempdir_in(dest).context("stage model download")?;
        let downloaded = match repo_api
            .download_file()
            .filename(&filename)
            .local_dir(staging.path())
            .progress(progress.clone())
            .send()
            .await
        {
            Ok(path) => path,
            Err(err) if optional => {
                tracing::debug!(%filename, %err, "skipping optional model file");
                continue;
            }
            Err(err) => {
                return Err(anyhow!("download {filename} from {repo}: {err}"));
            }
        };

        let bytes = std::fs::metadata(&downloaded)
            .with_context(|| format!("inspect downloaded model file {}", downloaded.display()))?
            .len();
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create model directory {}", parent.display()))?;
        }
        std::fs::rename(&downloaded, &target)
            .with_context(|| format!("install completed model file {}", target.display()))?;
        progress.advance_to(bytes);
        if let Ok(mut sink) = sink.lock() {
            sink.file_finished();
        }
        summary.bytes += bytes;
        summary.files.push(filename);
    }

    if summary.files.is_empty() {
        tracing::info!(repo, gguf_file, "model already present in the store");
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        started: Vec<(String, Option<u64>)>,
        bytes: u64,
        finished: usize,
    }

    impl ProgressSink for RecordingSink {
        fn file_started(&mut self, filename: &str, total_bytes: Option<u64>) {
            self.started.push((filename.to_string(), total_bytes));
        }
        fn bytes_advanced(&mut self, bytes: u64) {
            self.bytes += bytes;
        }
        fn file_finished(&mut self) {
            self.finished += 1;
        }
    }

    #[test]
    fn hub_progress_counts_overlapping_cumulative_events_once() {
        use hf_hub::progress::{FileProgress, FileStatus};
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let progress = HubProgress {
            sink: sink.clone(),
            filename: "model.safetensors".into(),
            completed: Mutex::new(0),
        };
        progress.on_progress(
            &DownloadEvent::Start {
                total_files: 1,
                total_bytes: 1024,
            }
            .into(),
        );
        progress.on_progress(
            &DownloadEvent::AggregateProgress {
                bytes_completed: 512,
                total_bytes: 1024,
                bytes_per_sec: None,
            }
            .into(),
        );
        progress.on_progress(
            &DownloadEvent::Progress {
                files: vec![FileProgress {
                    filename: "model.safetensors".into(),
                    bytes_completed: 512,
                    total_bytes: 1024,
                    status: FileStatus::InProgress,
                }],
            }
            .into(),
        );
        progress.advance_to(1024);
        progress.advance_to(512);
        let sink = sink.lock().unwrap();
        assert_eq!(sink.started, [("model.safetensors".into(), Some(1024))]);
        assert_eq!(sink.bytes, 1024);
        assert_eq!(sink.finished, 0, "the successful pull owns completion");
    }

    #[test]
    fn progress_continues_after_a_sink_panics() {
        struct PanickingSink;
        impl ProgressSink for PanickingSink {
            fn file_started(&mut self, _: &str, _: Option<u64>) {}
            fn bytes_advanced(&mut self, _: u64) {
                panic!("failed progress display");
            }
            fn file_finished(&mut self) {}
        }

        let progress = HubProgress {
            sink: Arc::new(Mutex::new(PanickingSink)),
            filename: "model.safetensors".into(),
            completed: Mutex::new(0),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            progress.advance_to(512);
        }));
        assert!(result.is_err());
        progress.advance_to(1024);
    }

    #[test]
    fn an_unknown_length_is_reported_as_indeterminate() {
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let progress = HubProgress {
            sink: sink.clone(),
            filename: "tokenizer.json".into(),
            completed: Mutex::new(0),
        };
        progress.on_progress(
            &DownloadEvent::Start {
                total_files: 1,
                total_bytes: 0,
            }
            .into(),
        );
        assert_eq!(sink.lock().unwrap().started[0].1, None);
    }

    struct HubFixture {
        server: Arc<tiny_http::Server>,
        worker: Option<std::thread::JoinHandle<()>>,
        requests: Arc<Mutex<Vec<String>>>,
        client: HFClient,
    }

    impl HubFixture {
        fn new(files: &[(&str, &[u8])], siblings: &[&str], truncate_get: bool) -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let client = HFClient::builder()
                .endpoint(format!("http://{}", server.server_addr()))
                .token("")
                .client(
                    reqwest::Client::builder()
                        // Make a deliberately short body close immediately, rather
                        // than waiting for the client's bounded fallback timeout.
                        .default_headers(reqwest::header::HeaderMap::from_iter([(
                            reqwest::header::CONNECTION,
                            reqwest::header::HeaderValue::from_static("close"),
                        )]))
                        .timeout(Duration::from_secs(10))
                        .build()
                        .unwrap(),
                )
                .cache_enabled(false)
                .retry_max_attempts(0)
                .build()
                .unwrap();
            let files: std::collections::HashMap<String, Vec<u8>> = files
                .iter()
                .map(|(name, bytes)| (format!("/test/model/resolve/main/{name}"), bytes.to_vec()))
                .collect();
            let listing = serde_json::json!({"id":"test/model", "siblings": siblings.iter()
                .map(|name| serde_json::json!({"rfilename":name})).collect::<Vec<_>>()})
            .to_string();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let observed = requests.clone();
            let receiver = server.clone();
            let worker = std::thread::spawn(move || {
                while let Ok(Some(request)) = receiver.recv_timeout(Duration::from_secs(10)) {
                    observed.lock().unwrap().push(format!(
                        "{} {}",
                        request.method(),
                        request.url()
                    ));
                    let response = if request.url().starts_with("/api/models/test/model") {
                        tiny_http::Response::from_data(listing.as_bytes())
                    } else if let Some(bytes) = files.get(request.url()) {
                        if truncate_get && request.method() == &tiny_http::Method::Get {
                            tiny_http::Response::new(
                                tiny_http::StatusCode(200),
                                vec![],
                                std::io::Cursor::new(bytes.clone()),
                                Some(bytes.len() + 100),
                                None,
                            )
                        } else {
                            tiny_http::Response::from_data(bytes.clone())
                        }
                    } else {
                        tiny_http::Response::from_data(Vec::new()).with_status_code(404)
                    };
                    let _ = request.respond(response);
                }
            });
            Self {
                server,
                worker: Some(worker),
                requests,
                client,
            }
        }
    }

    impl Drop for HubFixture {
        fn drop(&mut self) {
            self.server.unblock();
            self.worker.take().unwrap().join().unwrap();
        }
    }

    #[tokio::test]
    async fn gguf_pull_installs_nested_file_reports_progress_and_skips_installed_files() {
        let hub = HubFixture::new(&[("nested/model.gguf", b"model weights")], &[], false);
        let dest = tempfile::tempdir().unwrap();
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let summary = pull_into(
            "test/model",
            Some("nested/model.gguf"),
            dest.path(),
            sink.clone(),
            &hub.client,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(dest.path().join("nested/model.gguf")).unwrap(),
            b"model weights"
        );
        assert_eq!(
            summary,
            PullSummary {
                files: vec!["nested/model.gguf".into()],
                bytes: 13
            }
        );
        let count = hub.requests.lock().unwrap().len();
        let second = pull_into(
            "test/model",
            Some("nested/model.gguf"),
            dest.path(),
            sink.clone(),
            &hub.client,
        )
        .await
        .unwrap();
        assert_eq!(second, PullSummary::default());
        assert_eq!(
            hub.requests.lock().unwrap().len(),
            count,
            "installed files need no network"
        );
        let sink = sink.lock().unwrap();
        assert_eq!(sink.started, [("nested/model.gguf".into(), Some(13))]);
        assert_eq!(sink.bytes, 13);
        assert_eq!(sink.finished, 1);
    }

    #[tokio::test]
    async fn safetensors_pull_lists_shards_and_tolerates_missing_optional_files() {
        let hub = HubFixture::new(
            &[
                ("model.safetensors", b"weights"),
                ("config.json", b"{}"),
                ("tokenizer_config.json", b"{}"),
            ],
            &["model.safetensors", "README.md"],
            false,
        );
        let dest = tempfile::tempdir().unwrap();
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let summary = pull_into("test/model", None, dest.path(), sink.clone(), &hub.client)
            .await
            .unwrap();
        assert_eq!(
            summary.files,
            ["model.safetensors", "config.json", "tokenizer_config.json"]
        );
        assert_eq!(summary.bytes, 11);
        assert_eq!(sink.lock().unwrap().finished, 3);
        assert!(!dest.path().join("generation_config.json").exists());
    }

    #[tokio::test]
    async fn interrupted_download_never_leaves_an_installed_file_or_reports_completion() {
        let hub = HubFixture::new(&[("model.gguf", b"partial")], &[], true);
        let dest = tempfile::tempdir().unwrap();
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let result = pull_into(
            "test/model",
            Some("model.gguf"),
            dest.path(),
            sink.clone(),
            &hub.client,
        )
        .await;
        assert!(
            result.is_err(),
            "a truncated response must fail: {result:?}"
        );
        assert_eq!(
            std::fs::read_dir(dest.path()).unwrap().count(),
            0,
            "partial staging is removed"
        );
        assert_eq!(
            sink.lock().unwrap().bytes,
            7,
            "partial bytes reached the sink"
        );
        assert_eq!(sink.lock().unwrap().finished, 0);
    }

    #[tokio::test]
    async fn unsafe_listing_is_rejected_before_downloading_outside_the_store() {
        let hub = HubFixture::new(&[], &["../outside.safetensors"], false);
        let dest = tempfile::tempdir().unwrap();
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let error = pull_into("test/model", None, dest.path(), sink, &hub.client)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("only plain relative paths"),
            "{error}"
        );
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn missing_required_config_fails_without_discarding_completed_weights() {
        let hub = HubFixture::new(
            &[("model.safetensors", b"weights")],
            &["model.safetensors"],
            false,
        );
        let dest = tempfile::tempdir().unwrap();
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let error = pull_into("test/model", None, dest.path(), sink.clone(), &hub.client)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("config.json"), "{error}");
        assert_eq!(
            std::fs::read(dest.path().join("model.safetensors")).unwrap(),
            b"weights"
        );
        assert!(!dest.path().join("config.json").exists());
        assert_eq!(sink.lock().unwrap().finished, 1);
    }

    #[test]
    fn long_filenames_are_truncated_to_the_column() {
        assert_eq!(truncate("short", 28), "short");
        let long = truncate("a-very-long-safetensors-shard-name.safetensors", 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('…'));
    }
}

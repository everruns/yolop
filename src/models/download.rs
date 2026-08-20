//! Fetching weights into the model store, with visible progress.
//!
//! The engine can download its own weights, but it does so silently inside the
//! first inference call: a turn appears to hang for several gigabytes with
//! nothing on screen. Yolop pulls the files itself instead, so the wait is an
//! explicit `yolop weights pull` with a progress bar, and a turn either has its
//! weights or fails immediately with an actionable message.

use super::{GGUF_SEPARATOR, repo_dir, split_spec};
use anyhow::{Context, Result, anyhow};
use hf_hub::api::tokio::ApiBuilder;
use std::io::Write;
use std::path::PathBuf;
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

/// Adapter from `hf_hub`'s callback trait to our sink. `hf_hub` requires
/// `Clone + Send + Sync`, so the sink sits behind a shared lock rather than
/// being owned outright.
#[derive(Clone)]
struct HubProgress {
    sink: Arc<Mutex<dyn ProgressSink>>,
}

// The tokio flavour of `Progress` is async. Every call here is a lock and a
// formatted write, so the bodies stay synchronous inside an async signature
// rather than holding the lock across an await.
impl hf_hub::api::tokio::Progress for HubProgress {
    async fn init(&mut self, size: usize, filename: &str) {
        if let Ok(mut sink) = self.sink.lock() {
            sink.file_started(filename, (size > 0).then_some(size as u64));
        }
    }

    async fn update(&mut self, size: usize) {
        if let Ok(mut sink) = self.sink.lock() {
            sink.bytes_advanced(size as u64);
        }
    }

    async fn finish(&mut self) {
        if let Ok(mut sink) = self.sink.lock() {
            sink.file_finished();
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
/// A GGUF spec fetches the one named file plus the repo's tokenizer when it has
/// one; a safetensors spec fetches every `.safetensors` shard plus its support
/// files. Files already present are skipped, so an interrupted pull resumes at
/// file granularity.
pub async fn pull(spec: &str, sink: Arc<Mutex<dyn ProgressSink>>) -> Result<PullSummary> {
    let (repo, gguf_file) = split_spec(spec);
    let dest = repo_dir(repo)?;
    std::fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;

    let api = ApiBuilder::new()
        .with_progress(false)
        .build()
        .context("build the Hugging Face client")?;
    let repo_api = api.model(repo.to_string());

    let wanted = match gguf_file {
        Some(file) => vec![(file.to_string(), false)],
        None => {
            let info = repo_api
                .info()
                .await
                .with_context(|| format!("list files in {repo}"))?;
            let shards: Vec<(String, bool)> = info
                .siblings
                .iter()
                .map(|sibling| sibling.rfilename.clone())
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
        let target = super::safe_join(&dest, &filename)?;
        if target.is_file() {
            continue;
        }

        let progress = HubProgress { sink: sink.clone() };
        let downloaded = match repo_api.download_with_progress(&filename, progress).await {
            Ok(path) => path,
            Err(err) if optional => {
                tracing::debug!(%filename, %err, "skipping optional model file");
                continue;
            }
            Err(err) => {
                return Err(anyhow!("download {filename} from {repo}: {err}"));
            }
        };

        summary.bytes += install(&downloaded, &target)?;
        summary.files.push(filename);
    }

    if summary.files.is_empty() {
        tracing::info!(spec, "model already present in the store");
    }
    Ok(summary)
}

/// Move a downloaded file into the store, falling back to a copy when the hub
/// cache sits on a different filesystem (rename fails across mount points).
fn install(source: &PathBuf, target: &PathBuf) -> Result<u64> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(source, target) {
        Ok(()) => {}
        Err(_) => {
            std::fs::copy(source, target)
                .with_context(|| format!("copy into {}", target.display()))?;
        }
    }
    Ok(std::fs::metadata(target)?.len())
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

    #[tokio::test]
    async fn hub_callbacks_reach_the_sink() {
        use hf_hub::api::tokio::Progress;

        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let mut progress = HubProgress { sink: sink.clone() };
        progress.init(1024, "model.safetensors").await;
        progress.update(512).await;
        progress.update(512).await;
        progress.finish().await;

        let sink = sink.lock().expect("lock");
        assert_eq!(
            sink.started,
            vec![("model.safetensors".to_string(), Some(1024))]
        );
        assert_eq!(sink.bytes, 1024);
        assert_eq!(sink.finished, 1);
    }

    #[tokio::test]
    async fn an_unknown_length_is_reported_as_indeterminate() {
        use hf_hub::api::tokio::Progress;

        // Hugging Face omits the length for some files; a zero must not render
        // as a bar that can never fill.
        let sink = Arc::new(Mutex::new(RecordingSink::default()));
        let mut progress = HubProgress { sink: sink.clone() };
        progress.init(0, "tokenizer.json").await;
        assert_eq!(sink.lock().expect("lock").started[0].1, None);
    }

    #[test]
    fn optional_files_are_the_ones_repos_legitimately_omit() {
        assert!(is_optional("generation_config.json"));
        // Without a config the engine cannot construct the model at all, so a
        // missing one has to fail the pull rather than be skipped.
        assert!(!is_optional("config.json"));
    }

    #[test]
    fn install_falls_back_to_copy_across_filesystems() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("src.bin");
        let target = temp.path().join("nested").join("dst.bin");
        std::fs::write(&source, vec![7u8; 64]).expect("write");

        assert_eq!(install(&source, &target).expect("install"), 64);
        assert!(target.is_file());
    }

    #[test]
    fn long_filenames_are_truncated_to_the_column() {
        assert_eq!(truncate("short", 28), "short");
        let long = truncate("a-very-long-safetensors-shard-name.safetensors", 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('…'));
    }
}

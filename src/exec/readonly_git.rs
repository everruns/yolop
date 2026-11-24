//! Bounded read-only Git queries shared by runtime evidence and terminal views.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

const MAX_REVIEW_BYTES: usize = 512 * 1024;

pub(crate) async fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = tokio::process::Command::new("git")
        .arg("--no-pager")
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotePath=true"])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start Git")?;
    let mut stdout = child
        .stdout
        .take()
        .expect("piped stdout")
        .take(MAX_REVIEW_BYTES as u64 + 1);
    let mut stderr = child.stderr.take().expect("piped stderr").take(8193);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
        // A full pipe must never deadlock the wait below.
        if out.len() > MAX_REVIEW_BYTES || err.len() > 8192 {
            bail!("Git output exceeds the review limit; narrow the comparison");
        }
        let status = child.wait().await?;
        let differences = args.contains(&"--no-index") && status.code() == Some(1);
        if !status.success() && !differences {
            bail!("{}", String::from_utf8_lossy(&err).trim());
        }
        Ok(out)
    })
    .await
    .context("Git review timed out")?;
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

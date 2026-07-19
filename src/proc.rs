//! Subprocess execution that never corrupts the terminal the TUI owns.
//!
//! yolop's renderer owns stdout, and in `--fullscreen` the whole alternate
//! screen — which, unlike the inline viewport, has no scrollback to absorb
//! stray writes. Any child process that *inherits* our stdout/stderr therefore
//! prints straight onto the live frame: the `Preparing worktree …` /
//! `HEAD is now at …` / `From github.com…` corruption of the composer and
//! status bar.
//!
//! The fix is a policy, not a per-call-site patch: **internal subprocesses must
//! keep their output off the terminal.** This module is the single blessed way
//! to spawn them, so the rule is enforceable by "route it through `proc`"
//! instead of remembered (and forgotten) at each `Command`. Prefer these over a
//! bare `Command::status()`/`spawn()`, both of which inherit our stdio.
//!
//! Two shapes cover the internal cases:
//! - [`capture`] — run a one-shot command to completion with its output
//!   captured (git, cargo, …); the caller inspects status/stdout/stderr.
//! - [`detach_stdio`] — point a child's three streams at the null device for
//!   fire-and-forget launches (opening a browser) we neither read nor want on
//!   the frame.
//!
//! Long-running servers that speak a protocol over piped stdin/stdout (the
//! extension host, language servers) are already safe for those two streams;
//! the trap there is *stderr*, which must be piped-and-drained or nulled rather
//! than inherited (see `extensions::manager`).

use std::io;
use std::process::{Command, Output, Stdio};

/// Run `cmd` to completion with stdin detached and stdout/stderr **captured**
/// into the returned [`Output`], never inherited from the parent.
///
/// Use for one-shot internal commands whose output must stay off the terminal
/// but whose exit status or captured streams the caller needs (e.g. surfacing
/// stderr in an error message).
pub fn capture(cmd: &mut Command) -> io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Point a child's stdin, stdout, and stderr at the null device so nothing it
/// writes can reach the terminal. Returns the same `cmd` for chaining with
/// `.status()`/`.spawn()`.
///
/// Use for fire-and-forget launches (opening a URL in a browser) where we do
/// not consume the child's output.
pub fn detach_stdio(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_pipes_stdout_instead_of_inheriting_terminal() {
        // If `capture` inherited our stdio (as `Command::status()` does), the
        // returned stdout would be empty and the text would land on the
        // terminal — the exact leak that corrupts the --fullscreen frame.
        let mut cmd = Command::new("echo");
        cmd.arg("captured-not-leaked");
        let output = capture(&mut cmd).expect("run echo");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "captured-not-leaked"
        );
    }

    #[test]
    fn capture_also_captures_stderr() {
        // A command that writes only to stderr must still be captured, never
        // inherited — stderr is the stream git/cargo use for progress.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo to-stderr 1>&2"]);
        let output = capture(&mut cmd).expect("run sh");
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(String::from_utf8_lossy(&output.stderr).trim(), "to-stderr");
    }
}

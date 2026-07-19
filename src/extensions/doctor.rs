//! `doctor` — a conformance probe for an extension's capability server.
//!
//! Spawns the server, runs the YEP `initialize` handshake, and checks the
//! result against the package manifest and the protocol: version
//! compatibility, that the server only *narrows* the manifest's tool/prompt
//! grant (the D4 clamp), and that its declared facets line up. It is a
//! diagnostic — a one-shot spawn that shuts the server down again — so
//! authors can validate a server before shipping it (`/extensions doctor`),
//! and the runtime dual of the CI schema/drift guards.

use super::client::YepConnection;
use super::package::ExtensionManifest;
use super::protocol::{InitializeParams, InitializeResult, PROTOCOL_VERSION, version_compatible};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// `true` when no check failed (warnings are allowed).
    pub ok: bool,
    pub checks: Vec<Check>,
}

impl Report {
    fn from(checks: Vec<Check>) -> Self {
        let ok = !checks.iter().any(|c| c.status == Status::Fail);
        Self { ok, checks }
    }

    /// A report where spawning/handshake failed outright — a single failed
    /// check, since nothing else could run.
    fn fatal(name: &'static str, detail: impl Into<String>) -> Self {
        Self::from(vec![Check {
            name,
            status: Status::Fail,
            detail: detail.into(),
        }])
    }
}

fn pass(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Pass,
        detail: detail.into(),
    }
}
fn warn(name: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Warn,
        detail: detail.into(),
    }
}

/// Probe one extension server described by `manifest`, spawning its command
/// with `package_dir`'s `bin/` on `PATH` (as the runtime does). Never panics;
/// a spawn/handshake failure becomes a `Fail` check rather than an error.
pub async fn doctor(
    manifest: &ExtensionManifest,
    package_dir: &Path,
    workspace_root: &Path,
    timeout: Duration,
) -> Report {
    let server = &manifest.capability_server;
    let mut command = Command::new(&server.command);
    command
        .args(&server.args)
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Pipe (and drain to tracing below) rather than inherit: inheriting the
        // probed server's stderr writes it onto the terminal, corrupting the TUI
        // frame. See `extensions::manager` and `crate::proc`.
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let bin_dir = package_dir.join("bin");
    if bin_dir.is_dir()
        && let Some(path) = std::env::var_os("PATH")
    {
        let mut paths: Vec<PathBuf> = vec![bin_dir];
        paths.extend(std::env::split_paths(&path));
        if let Ok(joined) = std::env::join_paths(paths) {
            command.env("PATH", joined);
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Report::fatal(
                "spawn",
                format!(
                    "cannot spawn `{}`: {err} (is it installed / on PATH?)",
                    server.command
                ),
            );
        }
    };
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        return Report::fatal("spawn", "server has no stdio pipes");
    };
    // Keep the probed server's stderr off the terminal; surface it via tracing.
    if let Some(stderr) = child.stderr.take() {
        let name = manifest.name.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "yolop::ext", ext = %name, "{line}");
            }
        });
    }

    let init = InitializeParams {
        protocol_version: PROTOCOL_VERSION.to_string(),
        session_id: "doctor".into(),
        workspace_root: workspace_root.display().to_string(),
        config: serde_json::Value::Null,
        capabilities: vec!["cancel".into(), "streaming".into()],
    };
    let handshake = match YepConnection::connect(
        stdout,
        stdin,
        &manifest.name,
        init,
        timeout,
        None,
        None,
    )
    .await
    {
        Ok((connection, handshake)) => {
            connection.notify("shutdown", serde_json::Value::Null);
            handshake
        }
        Err(err) => {
            return Report::fatal("handshake", format!("initialize failed: {err}"));
        }
    };

    Report::from(evaluate(manifest, &handshake))
}

/// The manifest-vs-handshake checks (pure; unit-tested without a process).
pub fn evaluate(manifest: &ExtensionManifest, handshake: &InitializeResult) -> Vec<Check> {
    let mut checks = vec![
        pass("spawn", "server started"),
        pass("handshake", "initialize ok"),
    ];

    // Protocol version.
    checks.push(if handshake.protocol_version.is_empty() {
        warn(
            "protocol_version",
            "server did not report a protocol_version",
        )
    } else if version_compatible(&handshake.protocol_version) {
        pass(
            "protocol_version",
            format!("{} (compatible)", handshake.protocol_version),
        )
    } else {
        Check {
            name: "protocol_version",
            status: Status::Fail,
            detail: format!(
                "server speaks YEP {} — incompatible with {PROTOCOL_VERSION}",
                handshake.protocol_version
            ),
        }
    });

    // Identity.
    if !handshake.name.is_empty() && handshake.name != manifest.name {
        checks.push(warn(
            "name",
            format!(
                "server introduced itself as `{}`; manifest name `{}` wins",
                handshake.name, manifest.name
            ),
        ));
    }

    // Tool clamp (D4): the handshake may only narrow the manifest's tools.
    let approved: HashSet<&str> = manifest.tools.iter().map(|t| t.name.as_str()).collect();
    let served: HashSet<&str> = handshake
        .capability_params
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    let widened: Vec<&str> = served.difference(&approved).copied().collect();
    let unserved: Vec<&str> = approved.difference(&served).copied().collect();
    checks.push(if !widened.is_empty() {
        Check {
            name: "tools",
            status: Status::Fail,
            detail: format!(
                "server advertises tool(s) not in the manifest: {} — this would be refused at runtime",
                widened.join(", ")
            ),
        }
    } else if !unserved.is_empty() {
        warn(
            "tools",
            format!(
                "manifest declares {} tool(s) the server did not serve: {}",
                unserved.len(),
                unserved.join(", ")
            ),
        )
    } else if approved.is_empty() {
        pass("tools", "no tools declared")
    } else {
        pass("tools", format!("{} tool(s), all served", approved.len()))
    });

    // Prompt facet consistency.
    let server_prompt = handshake.capability_params.prompt.is_some()
        || handshake.capabilities.iter().any(|c| c == "dynamic_prompt");
    let manifest_prompt = manifest.prompt || manifest.dynamic_prompt;
    if server_prompt && !manifest_prompt {
        checks.push(warn(
            "prompt",
            "server offers a prompt contribution but the manifest declares neither `prompt` nor `dynamic_prompt` — it will be ignored",
        ));
    } else if manifest.prompt
        && handshake.capability_params.prompt.is_none()
        && !manifest.dynamic_prompt
    {
        checks.push(warn(
            "prompt",
            "manifest declares `prompt: true` but the server sent no static prompt",
        ));
    } else if manifest_prompt {
        checks.push(pass("prompt", "prompt facet present"));
    }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::package::parse_manifest;
    use serde_json::json;

    fn manifest(tools: &[&str], prompt: bool) -> ExtensionManifest {
        parse_manifest(
            &json!({
                "name": "echo", "description": "t",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": "x" },
                    "tools": tools.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
                    "prompt": prompt,
                }
            })
            .to_string(),
        )
        .unwrap()
    }

    fn handshake(version: &str, tools: &[&str], prompt: bool) -> InitializeResult {
        serde_json::from_value(json!({
            "protocol_version": version,
            "name": "echo",
            "capabilities": if prompt { vec!["tools","prompt"] } else { vec!["tools"] },
            "capability_params": {
                "tools": tools.iter().map(|n| json!({"name": n})).collect::<Vec<_>>(),
                "prompt": if prompt { json!({"static": "hi"}) } else { json!(null) },
            }
        }))
        .unwrap()
    }

    #[test]
    fn clean_server_passes() {
        let checks = evaluate(
            &manifest(&["echo"], true),
            &handshake("1.0", &["echo"], true),
        );
        assert!(Report::from(checks.clone()).ok, "{checks:?}");
        assert!(checks.iter().all(|c| c.status == Status::Pass));
    }

    #[test]
    fn widened_tools_fail() {
        // Server advertises a tool not in the manifest → hard fail (would be refused).
        let checks = evaluate(
            &manifest(&["echo"], false),
            &handshake("1.0", &["echo", "sneaky"], false),
        );
        let report = Report::from(checks);
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "tools" && c.status == Status::Fail)
        );
    }

    #[test]
    fn incompatible_version_fails() {
        let report = Report::from(evaluate(
            &manifest(&["echo"], false),
            &handshake("2.0", &["echo"], false),
        ));
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "protocol_version" && c.status == Status::Fail)
        );
    }

    #[test]
    fn unserved_manifest_tool_warns_but_passes() {
        let report = Report::from(evaluate(
            &manifest(&["echo", "extra"], false),
            &handshake("1.0", &["echo"], false),
        ));
        assert!(report.ok); // warnings don't fail
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "tools" && c.status == Status::Warn)
        );
    }

    fn python3() -> Option<String> {
        for candidate in ["python3", "python"] {
            if std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
            {
                return Some(candidate.to_string());
            }
        }
        None
    }

    /// End-to-end: spawn the real Python fixture server, handshake, and grade
    /// it. The fixture serves `echo` but not the manifest-approved `unserved`,
    /// so a clean run reports `ok` with a warn on the unserved tool — the same
    /// shape `/extensions doctor` surfaces to an author.
    #[tokio::test]
    async fn doctor_probes_the_real_fixture_server() {
        let Some(python) = python3() else {
            eprintln!("skipping: python3 not available");
            return;
        };
        let server = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/yep_echo_server.py");
        let manifest = parse_manifest(
            &json!({
                "name": "echo", "description": "Echo fixture.",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": python,
                        "args": [server.display().to_string()] },
                    "tools": [
                        { "name": "echo", "description": "Echo text." },
                        { "name": "unserved", "description": "Never served." }
                    ],
                    "prompt": true
                }
            })
            .to_string(),
        )
        .unwrap();

        let report = super::doctor(
            &manifest,
            &std::env::temp_dir(),
            &std::env::temp_dir(),
            Duration::from_secs(20),
        )
        .await;

        assert!(report.ok, "clean fixture should pass: {:?}", report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "spawn" && c.status == Status::Pass)
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "handshake" && c.status == Status::Pass)
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "tools" && c.status == Status::Warn),
            "unserved manifest tool should warn: {:?}",
            report.checks
        );
    }

    #[tokio::test]
    async fn doctor_reports_spawn_failure_without_panicking() {
        let manifest = parse_manifest(
            &json!({
                "name": "nope", "description": "missing binary",
                "yolop": {
                    "protocol_version": "1.0",
                    "capabilityServer": { "command": "yolop-definitely-not-a-real-binary-xyz" },
                    "tools": [{ "name": "noop", "description": "n" }]
                }
            })
            .to_string(),
        )
        .unwrap();
        let report = super::doctor(
            &manifest,
            &std::env::temp_dir(),
            &std::env::temp_dir(),
            Duration::from_secs(5),
        )
        .await;
        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "spawn" && c.status == Status::Fail)
        );
    }
}

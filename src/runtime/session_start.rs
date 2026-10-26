//! Session-start hook firing.
//!
//! Hook specs (workspace and global config plus capability contributions such
//! as the built-in `rex` handshake) are collected and finalized, but nothing
//! upstream invokes the session-start runner: `run_session_lifecycle_hooks`
//! has no production callers. This module closes that gap by firing
//! `SessionStart` hooks once per process, at the end of runtime construction,
//! so print mode, the interactive TUI, and serve all run them.
//!
//! Execution uses [`LocalShellDispatcher`], a host `sh` runner, because yolop
//! does not inject a sandbox hook dispatcher. Tool-use hooks keep whatever
//! dispatcher the host provides; only session-start uses this one.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use everruns_capabilities::capabilities::USER_HOOKS_CAPABILITY_ID;
use everruns_contracts::CapabilityRef;
use everruns_contracts::typed_id::SessionId;
use everruns_core::hook_executor::{BashExecOutput, BashHookDispatcher, ExecutorOpts, HookPayload};
use everruns_core::lifecycle_hooks::{
    SessionHookContext, build_session_lifecycle_hooks, run_session_lifecycle_hooks,
};
use everruns_core::user_hook_types::{HookEvent, UserHookSpec};
use everruns_core::{CapabilityRegistry, hook_adapter::finalize_hook_specs};

/// Payload exposed to session-start scripts, mirroring the upstream contract.
const PAYLOAD_JSON_VAR: &str = "EVERRUNS_HOOK_PAYLOAD_JSON";
/// Companion file for payloads too large or awkward for an env var.
const PAYLOAD_PATH_VAR: &str = "EVERRUNS_HOOK_PAYLOAD_PATH";

static PAYLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Runs hook commands on the host through `sh -c` (or `cmd /C` on Windows).
///
/// Honors the spec timeout and truncates each stream at `max_output_bytes`.
/// This dispatcher is deliberately scoped to session-start: it has no sandbox,
/// it runs with the process environment and working directory.
pub(crate) struct LocalShellDispatcher;

#[async_trait]
impl BashHookDispatcher for LocalShellDispatcher {
    async fn dispatch(
        &self,
        payload: &HookPayload,
        command: &str,
        extra_env: &BTreeMap<String, String>,
        opts: &ExecutorOpts,
    ) -> Result<BashExecOutput, String> {
        let payload_json =
            serde_json::to_string(payload).map_err(|err| format!("hook payload: {err}"))?;
        let payload_path = write_payload_file(&payload_json);

        #[cfg(windows)]
        let (shell, flag) = ("cmd", "/C");
        #[cfg(not(windows))]
        let (shell, flag) = ("sh", "-c");

        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg(flag)
            .arg(command)
            .env(PAYLOAD_JSON_VAR, &payload_json)
            .envs(extra_env);
        if let Some(path) = &payload_path {
            cmd.env(PAYLOAD_PATH_VAR, path);
        }
        // Kill the child on timeout so an expired hook cannot outlive the
        // wait and keep running detached.
        cmd.kill_on_drop(true);

        let mut child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|err| format!("session-start hook failed to spawn: {err}"))?;
        // Bounded reads: one byte past the cap tells truncation apart,
        // contents are trimmed below.
        let cap = opts.max_output_bytes.saturating_add(1) as u64;
        // Drain both pipes concurrently with the wait: a child that fills a
        // pipe buffer must never block the wait from completing.
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let mut out_handle = tokio::spawn(async move { drain_capped(out_pipe, cap).await });
        let mut err_handle = tokio::spawn(async move { drain_capped(err_pipe, cap).await });
        let run = async {
            let status = child.wait().await?;
            let out = (&mut out_handle).await.map_err(std::io::Error::other)??;
            let err = (&mut err_handle).await.map_err(std::io::Error::other)??;
            Ok::<_, std::io::Error>((status, out, err))
        };
        let (status, stdout, stderr) =
            match tokio::time::timeout(Duration::from_millis(opts.timeout_ms.into()), run).await {
                Ok(Ok(output)) => output,
                Ok(Err(err)) => {
                    let _ = payload_path.map(std::fs::remove_file);
                    return Err(format!("session-start hook failed while running: {err}"));
                }
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = payload_path.map(std::fs::remove_file);
                    return Err(format!(
                        "session-start hook timed out after {}ms",
                        opts.timeout_ms
                    ));
                }
            };

        let _ = payload_path.map(std::fs::remove_file);
        Ok(BashExecOutput {
            exit_code: status.code().unwrap_or(-1),
            stdout: truncate(&String::from_utf8_lossy(&stdout), opts.max_output_bytes),
            stderr: truncate(&String::from_utf8_lossy(&stderr), opts.max_output_bytes),
        })
    }
}

/// Best-effort payload sidecar; hooks still get the JSON env var when the
/// file cannot be written. Created with `O_EXCL` and owner-only permissions
/// so a predictable temp path cannot be swapped for a symlink.
fn write_payload_file(payload_json: &str) -> Option<String> {
    for _ in 0..8 {
        let path = std::env::temp_dir().join(format!(
            "yolop-hook-payload-{}-{}.json",
            std::process::id(),
            PAYLOAD_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                use std::io::Write as _;
                if file.write_all(payload_json.as_bytes()).is_ok() {
                    return Some(path.to_string_lossy().into_owned());
                }
                let _ = std::fs::remove_file(&path);
                return None;
            }
            Err(_) => continue,
        }
    }
    None
}

/// Read at most `cap` bytes, then drain and discard the rest so the writer
/// observes EOF instead of SIGPIPE and exits normally. Memory stays bounded:
/// only the capped prefix is kept.
async fn drain_capped(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    cap: u64,
) -> Result<Vec<u8>, std::io::Error> {
    use tokio::io::AsyncReadExt as _;
    let Some(pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut kept = Vec::new();
    let mut limited = pipe.take(cap);
    limited.read_to_end(&mut kept).await?;
    let mut rest = limited.into_inner();
    tokio::io::copy(&mut rest, &mut tokio::io::sink()).await?;
    Ok(kept)
}

fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Collect finalized `SessionStart` specs from config files and capability
/// contributions (for example the built-in `rex` handshake), honoring
/// `disabled_contributions` from the user-hooks config.
pub(crate) fn collect_session_start_specs(
    registry: &CapabilityRegistry,
    refs: &[CapabilityRef],
) -> Vec<UserHookSpec> {
    let mut contributions = Vec::new();
    let mut disabled: Vec<String> = Vec::new();
    for reference in refs {
        // The disabled list rides on the user-hooks config, which applies
        // even without consulting the registry.
        if reference.id() == USER_HOOKS_CAPABILITY_ID
            && let Some(entries) = reference
                .config_value()
                .get("disabled_contributions")
                .and_then(serde_json::Value::as_array)
        {
            disabled.extend(
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string),
            );
        }
        let Some(capability) = registry.get(reference.id()) else {
            continue;
        };
        let specs = capability.user_hooks_with_config(reference.config_value());
        if !specs.is_empty() {
            contributions.push((reference.id().to_string(), specs));
        }
    }
    let mut specs = finalize_hook_specs(contributions, &disabled);
    specs.retain(|spec| matches!(spec.event, HookEvent::SessionStart));
    specs
}

/// Fire all `SessionStart` hooks once. Advisory only: hook failures are
/// recorded by the hook itself (warn/allow semantics) and never abort startup.
pub(crate) async fn fire_session_start_hooks(
    registry: &CapabilityRegistry,
    refs: &[CapabilityRef],
    session_id: SessionId,
) {
    let specs = collect_session_start_specs(registry, refs);
    if specs.is_empty() {
        return;
    }
    let dispatcher: Arc<dyn BashHookDispatcher> = Arc::new(LocalShellDispatcher);
    let hooks = build_session_lifecycle_hooks(&specs, HookEvent::SessionStart, dispatcher);
    let ctx = SessionHookContext {
        session_id,
        org_id: None,
        agent_id: Some("yolop".to_string()),
    };
    run_session_lifecycle_hooks(&hooks, &ctx, serde_json::json!({"agent": "yolop"})).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use everruns_core::Capability;
    use everruns_core::user_hook_types::HookId;
    use std::collections::BTreeMap;

    struct StubSessionCapability {
        available: bool,
    }

    #[async_trait]
    impl Capability for StubSessionCapability {
        fn id(&self) -> &'static str {
            "stub-session"
        }

        fn name(&self) -> &'static str {
            "Stub session"
        }

        fn description(&self) -> &'static str {
            "Test session-start contributor."
        }

        fn user_hooks(&self) -> Vec<UserHookSpec> {
            if !self.available {
                return Vec::new();
            }
            vec![UserHookSpec {
                id: Some("handshake".to_string()),
                event: HookEvent::SessionStart,
                matcher: Default::default(),
                executor: everruns_core::user_hook_types::ExecutorSpec::Bash {
                    command: "stub handshake".to_string(),
                    env: BTreeMap::new(),
                },
                timeout_ms: 5_000,
                on_error: everruns_core::user_hook_types::OnError::Allow,
                description: None,
                source: everruns_core::user_hook_types::HookSource::Capability {
                    capability_id: "stub-session".to_string(),
                },
            }]
        }
    }

    fn test_registry(available: bool) -> CapabilityRegistry {
        let mut registry = CapabilityRegistry::new();
        registry.register(StubSessionCapability { available });
        registry
    }

    #[test]
    fn collects_session_start_contributions() {
        let registry = test_registry(true);
        let refs = vec![CapabilityRef::new("stub-session")];

        let specs = collect_session_start_specs(&registry, &refs);

        assert_eq!(specs.len(), 1);
        assert!(matches!(specs[0].event, HookEvent::SessionStart));
    }

    #[test]
    fn respects_disabled_contributions() {
        let registry = test_registry(true);
        let refs = vec![
            CapabilityRef::new("stub-session"),
            CapabilityRef::with_config(
                USER_HOOKS_CAPABILITY_ID,
                serde_json::json!({"disabled_contributions": ["stub-session:handshake"]}),
            ),
        ];

        let specs = collect_session_start_specs(&registry, &refs);

        assert!(specs.is_empty());
    }

    #[test]
    fn skips_unavailable_contributors() {
        let registry = test_registry(false);
        let refs = vec![CapabilityRef::new("stub-session")];

        assert!(collect_session_start_specs(&registry, &refs).is_empty());
    }

    #[tokio::test]
    async fn dispatcher_runs_shell_commands() {
        let dispatcher = LocalShellDispatcher;
        let payload = HookPayload {
            event: HookEvent::SessionStart,
            hook_id: HookId("test".to_string()),
            session_id: SessionId::new(),
            turn_id: None,
            org_id: None,
            agent_id: None,
            ts: String::new(),
            data: serde_json::Value::Null,
        };

        let out = dispatcher
            .dispatch(
                &payload,
                "echo hello-from-hook",
                &BTreeMap::new(),
                &ExecutorOpts::default(),
            )
            .await
            .expect("echo hook runs");

        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("hello-from-hook"));
    }

    #[tokio::test]
    async fn dispatcher_drains_large_output_before_exit() {
        // ~1.4MB of stdout would fill a pipe buffer and deadlock a
        // wait-then-read implementation.
        let dispatcher = LocalShellDispatcher;
        let payload = HookPayload {
            event: HookEvent::SessionStart,
            hook_id: HookId("test".to_string()),
            session_id: SessionId::new(),
            turn_id: None,
            org_id: None,
            agent_id: None,
            ts: String::new(),
            data: serde_json::Value::Null,
        };

        let out = dispatcher
            .dispatch(
                &payload,
                "seq 1 200000",
                &BTreeMap::new(),
                &ExecutorOpts::default(),
            )
            .await
            .expect("large output drains");

        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.len() <= 65536);
    }

    #[tokio::test]
    async fn dispatcher_enforces_timeout() {
        let dispatcher = LocalShellDispatcher;
        let payload = HookPayload {
            event: HookEvent::SessionStart,
            hook_id: HookId("test".to_string()),
            session_id: SessionId::new(),
            turn_id: None,
            org_id: None,
            agent_id: None,
            ts: String::new(),
            data: serde_json::Value::Null,
        };
        let opts = ExecutorOpts {
            timeout_ms: 50,
            ..Default::default()
        };

        let err = dispatcher
            .dispatch(&payload, "sleep 30", &BTreeMap::new(), &opts)
            .await
            .expect_err("sleep exceeds the timeout");

        assert!(err.contains("timed out"), "unexpected error: {err}");
    }
}

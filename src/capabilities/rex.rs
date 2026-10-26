//! The `rex` capability: session-start handshake with the rex daemon.
//!
//! When a runnable `rex` binary resolves on `PATH`, yolop contributes a
//! built-in `SessionStart` hook that runs
//! `rex __internal session-start --agent yolop`. This mirrors the Codex
//! `hooks.json` recipe (`rex __internal session-start --agent codex`, five
//! second timeout), with the agent name changed to yolop. The hook is best
//! effort: output is captured, never shown, and a nonzero exit still counts
//! as success (`OnError::Allow`), so a failing daemon never breaks session
//! startup. With no `rex` on `PATH` the capability contributes nothing.

use std::collections::BTreeMap;
use std::ffi::OsString;

use async_trait::async_trait;
use everruns_core::Capability;
use everruns_core::user_hook_types::{
    ExecutorSpec, HookEvent, HookMatcher, HookSource, OnError, UserHookSpec,
};

pub(crate) const REX_CAPABILITY_ID: &str = "rex";

/// Session-start handshake. Output is captured by the hook runner and failure
/// is tolerated via `OnError::Allow`, so no shell redirection is needed:
/// the same command runs under `sh -c` and `cmd /C`.
const REX_SESSION_START_COMMAND: &str = "rex __internal session-start --agent yolop";
/// Same five second budget as the Codex recipe (`"timeout": 5`).
const REX_SESSION_START_TIMEOUT_MS: u32 = 5_000;

#[cfg(windows)]
const REX_BINARY_NAMES: [&str; 2] = ["rex.exe", "rex"];
#[cfg(not(windows))]
const REX_BINARY_NAMES: [&str; 1] = ["rex"];

/// Session-start hook for the rex daemon. Always registered and in the default
/// harness; it only contributes hooks when `rex` resolves on `PATH`.
pub(crate) struct RexCapability {
    available: bool,
}

impl RexCapability {
    pub(crate) fn new() -> Self {
        Self {
            available: std::env::var_os("PATH").is_some_and(|var| rex_found_on_search_path(&var)),
        }
    }

    fn session_start_hooks(available: bool) -> Vec<UserHookSpec> {
        if !available {
            return Vec::new();
        }
        vec![UserHookSpec {
            id: None,
            event: HookEvent::SessionStart,
            matcher: HookMatcher::default(),
            executor: ExecutorSpec::Bash {
                command: REX_SESSION_START_COMMAND.to_string(),
                env: BTreeMap::new(),
            },
            timeout_ms: REX_SESSION_START_TIMEOUT_MS,
            on_error: OnError::Allow,
            description: Some("Rex session-start handshake.".to_string()),
            source: HookSource::Capability {
                capability_id: REX_CAPABILITY_ID.to_string(),
            },
        }]
    }
}

fn rex_found_on_search_path(path_var: &OsString) -> bool {
    REX_BINARY_NAMES.iter().any(|binary| {
        std::env::split_paths(path_var)
            .map(|dir| dir.join(binary))
            .any(|candidate| is_runnable_file(&candidate))
    })
}

/// A `rex` binary counts as present when it exists and, on Unix, carries an
/// execute bit, mirroring the `rg` availability probe.
fn is_runnable_file(candidate: &std::path::Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        candidate
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[async_trait]
impl Capability for RexCapability {
    fn id(&self) -> &'static str {
        REX_CAPABILITY_ID
    }

    fn name(&self) -> &'static str {
        "Rex"
    }

    fn description(&self) -> &'static str {
        "Runs the rex session-start handshake when rex is on PATH."
    }

    fn user_hooks(&self) -> Vec<UserHookSpec> {
        Self::session_start_hooks(self.available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Keep PATH lookups hermetic in tests: this goes through [`Path`], never
    /// the live process environment.
    fn search_path_with_entry(dir: &Path, extra: Option<&str>) -> OsString {
        let mut entries = vec![dir.as_os_str().to_owned()];
        if let Some(extra) = extra {
            entries.push(extra.into());
        }
        std::env::join_paths(entries).expect("join test search path")
    }

    fn hook_command(hook: &UserHookSpec) -> &str {
        match &hook.executor {
            ExecutorSpec::Bash { command, .. } => command,
        }
    }

    #[test]
    fn session_start_hook_hands_off_to_rex_as_yolop() {
        let hooks = RexCapability::session_start_hooks(true);

        assert_eq!(hooks.len(), 1, "rex contributes one SessionStart hook");
        let hook = &hooks[0];
        assert!(matches!(hook.event, HookEvent::SessionStart));
        assert!(
            hook_command(hook).contains("rex __internal session-start --agent yolop"),
            "handshake must identify as yolop, got: {}",
            hook_command(hook)
        );
        assert_eq!(hook.timeout_ms, 5_000);
    }

    #[test]
    fn no_hooks_without_rex_on_path() {
        assert!(RexCapability::session_start_hooks(false).is_empty());
    }

    #[test]
    fn finds_rex_binary_on_search_path() {
        let dir = std::env::temp_dir().join(format!("yolop-rex-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create probe dir");
        let binary = dir.join(REX_BINARY_NAMES[0]);
        std::fs::write(&binary, "#!/bin/sh\n").expect("write fake rex");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                .expect("make fake rex executable");
        }

        assert!(rex_found_on_search_path(&search_path_with_entry(
            &dir, None
        )));
        assert!(!rex_found_on_search_path(&search_path_with_entry(
            &dir.join("missing"),
            None
        )));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let inert = dir.join("inert");
            std::fs::create_dir_all(&inert).expect("probe dir");
            std::fs::write(inert.join(REX_BINARY_NAMES[0]), "#!/bin/sh\n")
                .expect("write non-executable fake");
            std::fs::set_permissions(
                inert.join(REX_BINARY_NAMES[0]),
                std::fs::Permissions::from_mode(0o644),
            )
            .expect("strip executable bit");
            assert!(!rex_found_on_search_path(&search_path_with_entry(
                &inert, None
            )));
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}

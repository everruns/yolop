//! Herdr environment integration.
//!
//! Herdr injects a local socket path and stable pane id into every managed
//! process. When that contract is present, yolop reports its turn lifecycle and
//! exposes a read-only Herdr skill through the existing skills capability. The
//! integration never fetches instructions or writes a user skill directory.

use async_trait::async_trait;
use everruns_core::capabilities::{Capability, CapabilityStatus, SystemPromptContext};
use everruns_core::{Event, EventData, TURN_CANCELLED, TURN_COMPLETED, TURN_FAILED, TURN_STARTED};
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

pub(crate) const HERDR_CAPABILITY_ID: &str = "herdr";
const HERDR_SOURCE: &str = "yolop:lifecycle";
const HERDR_METADATA_SOURCE: &str = "yolop:metadata";
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);

const HERDR_SKILL_MD: &str = r#"---
name: herdr
description: Control the current Herdr terminal session when the user explicitly asks to inspect or operate Herdr panes, tabs, workspaces, or agents.
---

# Herdr

This yolop process is running inside a Herdr-managed pane. Use Herdr only when
the user explicitly asks for Herdr coordination or inspection; do not introduce
extra panes or agents merely because parallel work is possible.

The installed `herdr` binary is the authority for syntax. Inspect a command
group such as `herdr pane`, `herdr agent`, `herdr workspace`, `herdr tab`, or
`herdr wait`; do not run bare `herdr` for discovery because it attaches the UI.

Use `$HERDR_PANE_ID` or `--current` for this pane. Treat all returned IDs as
opaque and read them from command JSON. For background work, preserve the
user's focus with `--no-focus`. Read current output before waiting for future
output. Never stop the Herdr server, close resources you did not create, or
kill another pane's process unless the user explicitly requests it.
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HerdrState {
    Idle,
    Working,
    Blocked,
}

impl HerdrState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug)]
struct HerdrContext {
    bin: OsString,
    pane_id: String,
    socket_path: OsString,
    agent_session_id: String,
    sequence: AtomicU64,
    release_on_drop: bool,
}

impl HerdrContext {
    fn from_lookup(
        agent_session_id: String,
        release_on_drop: bool,
        mut get: impl FnMut(&str) -> Option<OsString>,
    ) -> Option<Self> {
        if get("HERDR_ENV").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return None;
        }
        let pane_id = get("HERDR_PANE_ID")?.into_string().ok()?;
        if pane_id.trim().is_empty() {
            return None;
        }
        let socket_path = get("HERDR_SOCKET_PATH")?;
        if socket_path.is_empty() {
            return None;
        }
        let bin = get("HERDR_BIN_PATH")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("herdr"));
        Some(Self {
            bin,
            pane_id,
            socket_path,
            agent_session_id,
            sequence: AtomicU64::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros()
                    .min(u64::MAX as u128) as u64,
            ),
            release_on_drop,
        })
    }

    fn command_args(&self, state: HerdrState, sequence: u64) -> Vec<OsString> {
        [
            "pane".into(),
            "report-agent".into(),
            self.pane_id.clone().into(),
            "--source".into(),
            HERDR_SOURCE.into(),
            "--agent".into(),
            "yolop".into(),
            "--state".into(),
            state.as_str().into(),
            "--seq".into(),
            sequence.to_string().into(),
            "--agent-session-id".into(),
            self.agent_session_id.clone().into(),
        ]
        .into()
    }

    fn display_agent(&self, title: Option<&str>) -> String {
        let label = title
            .filter(|title| !title.trim().is_empty())
            .map(str::trim)
            .unwrap_or_else(|| {
                let id = self
                    .agent_session_id
                    .rsplit('_')
                    .next()
                    .unwrap_or(&self.agent_session_id);
                &id[id.len().saturating_sub(8)..]
            });
        format!("Yolop · {label}")
    }

    fn metadata_args(&self, title: Option<&str>, sequence: u64) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "pane".into(),
            "report-metadata".into(),
            self.pane_id.clone().into(),
            "--source".into(),
            HERDR_METADATA_SOURCE.into(),
            "--agent".into(),
            "yolop".into(),
            "--applies-to-source".into(),
            HERDR_SOURCE.into(),
            "--display-agent".into(),
            self.display_agent(title).into(),
            "--state-label".into(),
            "idle=Ready".into(),
            "--state-label".into(),
            "working=Working".into(),
            "--state-label".into(),
            "blocked=Needs input".into(),
        ]
        .into();
        if let Some(title) = title {
            args.extend([OsString::from("--title"), title.into()]);
        }
        args.extend([OsString::from("--seq"), sequence.to_string().into()]);
        args
    }

    fn release_args(&self, sequence: u64) -> Vec<OsString> {
        [
            "pane".into(),
            "release-agent".into(),
            self.pane_id.clone().into(),
            "--source".into(),
            HERDR_SOURCE.into(),
            "--agent".into(),
            "yolop".into(),
            "--seq".into(),
            sequence.to_string().into(),
        ]
        .into()
    }
}

impl Drop for HerdrContext {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        // This is the last reporter owner and may run while Tokio itself is
        // shutting down. Spawn the direct CLI command instead of depending on
        // an async task that the runtime could cancel before it reaches Herdr.
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        match std::process::Command::new(&self.bin)
            .args(self.release_args(sequence))
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", &self.pane_id)
            .env("HERDR_SOCKET_PATH", &self.socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + REPORT_TIMEOUT;
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) if std::time::Instant::now() < deadline => {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Ok(None) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                break;
                            }
                            Err(error) => {
                                tracing::debug!(%error, "Herdr agent release wait failed");
                                break;
                            }
                        }
                    }
                });
            }
            Err(error) => tracing::debug!(%error, "Herdr agent release failed"),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HerdrReporter {
    context: Option<Arc<HerdrContext>>,
}

impl HerdrReporter {
    pub(crate) fn from_env(agent_session_id: impl Into<String>) -> Self {
        Self::from_lookup_inner(agent_session_id.into(), true, |key| std::env::var_os(key))
    }

    #[cfg(test)]
    fn from_lookup(agent_session_id: String, get: impl FnMut(&str) -> Option<OsString>) -> Self {
        Self::from_lookup_inner(agent_session_id, false, get)
    }

    fn from_lookup_inner(
        agent_session_id: String,
        release_on_drop: bool,
        get: impl FnMut(&str) -> Option<OsString>,
    ) -> Self {
        Self {
            context: HerdrContext::from_lookup(agent_session_id, release_on_drop, get)
                .map(Arc::new),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.context.is_some()
    }

    pub(crate) fn report_background(&self, state: HerdrState) {
        if self.context.is_none() {
            return;
        }
        let reporter = self.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { reporter.report(state).await });
        }
    }

    async fn report(&self, state: HerdrState) {
        let Some(context) = self.context.clone() else {
            return;
        };
        let sequence = context.sequence.fetch_add(1, Ordering::Relaxed);
        let mut command = tokio::process::Command::new(&context.bin);
        command
            .args(context.command_args(state, sequence))
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", &context.pane_id)
            .env("HERDR_SOCKET_PATH", &context.socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(REPORT_TIMEOUT, command.status()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => tracing::debug!(%status, ?state, "Herdr state report failed"),
            Ok(Err(error)) => tracing::debug!(%error, ?state, "Herdr state report failed"),
            Err(_) => tracing::debug!(?state, "Herdr state report timed out"),
        }
    }

    async fn report_metadata(&self, title: Option<&str>) {
        let Some(context) = self.context.clone() else {
            return;
        };
        let sequence = context.sequence.fetch_add(1, Ordering::Relaxed);
        let mut command = tokio::process::Command::new(&context.bin);
        command
            .args(context.metadata_args(title, sequence))
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", &context.pane_id)
            .env("HERDR_SOCKET_PATH", &context.socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(REPORT_TIMEOUT, command.status()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => tracing::debug!(%status, "Herdr metadata report failed"),
            Ok(Err(error)) => tracing::debug!(%error, "Herdr metadata report failed"),
            Err(_) => tracing::debug!("Herdr metadata report timed out"),
        }
    }

    pub(crate) fn start_monitor(
        &self,
        session_id: everruns_core::typed_id::SessionId,
        mut events: broadcast::Receiver<Event>,
    ) {
        if self.context.is_none() {
            return;
        }
        let reporter = self.clone();
        tokio::spawn(async move {
            reporter.report_metadata(None).await;
            reporter.report(HerdrState::Idle).await;
            loop {
                match events.recv().await {
                    Ok(event) if event.session_id == session_id => match &event.data {
                        EventData::SessionTitleUpdated(data) => {
                            reporter.report_metadata(Some(&data.title)).await
                        }
                        _ => match event.event_type.as_str() {
                            TURN_STARTED => reporter.report(HerdrState::Working).await,
                            TURN_COMPLETED | TURN_FAILED | TURN_CANCELLED => {
                                reporter.report(HerdrState::Idle).await
                            }
                            _ => {}
                        },
                    },
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

pub(crate) struct HerdrCapability {
    active: bool,
}

impl HerdrCapability {
    pub(crate) fn new(active: bool) -> Self {
        Self { active }
    }

    pub(crate) fn skill_content(active: bool) -> Option<&'static str> {
        active.then_some(HERDR_SKILL_MD)
    }
}

#[async_trait]
impl Capability for HerdrCapability {
    fn id(&self) -> &str {
        HERDR_CAPABILITY_ID
    }

    fn name(&self) -> &str {
        "Herdr environment"
    }

    fn description(&self) -> &str {
        "Reports Yolop lifecycle state and mounts Herdr operating guidance inside Herdr panes."
    }

    fn status(&self) -> CapabilityStatus {
        CapabilityStatus::Available
    }

    fn category(&self) -> Option<&str> {
        Some("Integrations")
    }

    async fn system_prompt_contribution(&self, _ctx: &SystemPromptContext) -> Option<String> {
        self.active.then(|| {
            "<capability id=\"herdr\">\nThis process is inside Herdr. A read-only `herdr` \
             skill is available; activate it only when the user explicitly asks to inspect or \
             control Herdr.\n</capability>"
                .to_string()
        })
    }

    fn dependencies(&self) -> Vec<&'static str> {
        vec![everruns_core::capabilities::SESSION_FILE_SYSTEM_CAPABILITY_ID]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn reporter(vars: &[(&str, &str)]) -> HerdrReporter {
        let vars: HashMap<&str, OsString> = vars
            .iter()
            .map(|(key, value)| (*key, OsString::from(value)))
            .collect();
        HerdrReporter::from_lookup("session_test".into(), |key| vars.get(key).cloned())
    }

    #[test]
    fn requires_complete_herdr_environment_contract() {
        assert!(!reporter(&[]).is_active());
        assert!(!reporter(&[("HERDR_ENV", "1")]).is_active());
        assert!(
            reporter(&[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w1:p2"),
                ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
            ])
            .is_active()
        );
    }

    #[test]
    fn state_report_is_a_direct_cli_invocation_with_session_identity() {
        let reporter = reporter(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p2"),
            ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
        ]);
        let context = reporter.context.expect("active context");
        let args = context.command_args(HerdrState::Blocked, 42);
        let args: Vec<String> = args
            .into_iter()
            .map(|arg| arg.into_string().expect("utf-8 test argument"))
            .collect();

        assert_eq!(
            args,
            vec![
                "pane",
                "report-agent",
                "w1:p2",
                "--source",
                "yolop:lifecycle",
                "--agent",
                "yolop",
                "--state",
                "blocked",
                "--seq",
                "42",
                "--agent-session-id",
                "session_test",
            ]
        );
    }

    #[test]
    fn metadata_distinguishes_sessions_with_title_or_stable_suffix() {
        let reporter = reporter(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p2"),
            ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
        ]);
        let context = reporter.context.expect("active context");

        assert_eq!(context.display_agent(None), "Yolop · test");
        assert_eq!(
            context.display_agent(Some("  Improve Herdr integration  ")),
            "Yolop · Improve Herdr integration"
        );

        let args: Vec<String> = context
            .metadata_args(Some("Improve Herdr integration"), 44)
            .into_iter()
            .map(|arg| arg.into_string().expect("utf-8 test argument"))
            .collect();
        assert_eq!(
            args,
            vec![
                "pane",
                "report-metadata",
                "w1:p2",
                "--source",
                "yolop:metadata",
                "--agent",
                "yolop",
                "--applies-to-source",
                "yolop:lifecycle",
                "--display-agent",
                "Yolop · Improve Herdr integration",
                "--state-label",
                "idle=Ready",
                "--state-label",
                "working=Working",
                "--state-label",
                "blocked=Needs input",
                "--title",
                "Improve Herdr integration",
                "--seq",
                "44",
            ]
        );
    }

    #[test]
    fn release_targets_only_yolops_own_agent_source() {
        let reporter = reporter(&[
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p2"),
            ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
        ]);
        let context = reporter.context.as_ref().expect("active context");
        let args: Vec<String> = context
            .release_args(43)
            .into_iter()
            .map(|arg| arg.into_string().expect("utf-8 test argument"))
            .collect();

        assert_eq!(
            args,
            vec![
                "pane",
                "release-agent",
                "w1:p2",
                "--source",
                "yolop:lifecycle",
                "--agent",
                "yolop",
                "--seq",
                "43",
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn monitor_reports_turn_lifecycle_and_releases_the_agent() {
        use everruns_core::events::{
            EventContext, SessionTitleUpdatedData, TurnCompletedData, TurnStartedData,
        };
        use everruns_core::typed_id::{MessageId, TurnId};
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let cli = temp.path().join("herdr-test");
        let calls = temp.path().join("herdr-test.calls");
        std::fs::write(&cli, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${0}.calls\"\n")
            .expect("write fake Herdr CLI");
        let mut permissions = std::fs::metadata(&cli).expect("CLI metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&cli, permissions).expect("make CLI executable");

        let vars = HashMap::from([
            ("HERDR_ENV", OsString::from("1")),
            ("HERDR_PANE_ID", OsString::from("w1:p2")),
            ("HERDR_SOCKET_PATH", OsString::from("/tmp/herdr.sock")),
            ("HERDR_BIN_PATH", cli.as_os_str().to_os_string()),
        ]);
        let reporter = HerdrReporter::from_lookup_inner("session_test".into(), true, |key| {
            vars.get(key).cloned()
        });
        let session_id = everruns_core::typed_id::SessionId::from_seed(91);
        let turn_id = TurnId::from_seed(92);
        let (events, receiver) = broadcast::channel(8);
        reporter.start_monitor(session_id, receiver);
        events
            .send(Event::new(
                session_id,
                EventContext::default(),
                SessionTitleUpdatedData {
                    previous_title: None,
                    title: "Improve Herdr integration".to_string(),
                },
            ))
            .expect("send session title updated");
        events
            .send(Event::new(
                session_id,
                EventContext::default(),
                TurnStartedData {
                    turn_id,
                    input_message_id: MessageId::from_seed(93),
                    input_content: Some("test".to_string()),
                },
            ))
            .expect("send turn started");
        events
            .send(Event::new(
                session_id,
                EventContext::default(),
                TurnCompletedData {
                    turn_id,
                    iterations: 1,
                    duration_ms: Some(1),
                    usage: None,
                    input_content: Some("test".to_string()),
                    final_message_id: None,
                    final_answer_preview: None,
                    time_to_first_token_ms: None,
                    tool_call_count: Some(0),
                    llm_call_count: Some(1),
                    status: Some("completed".to_string()),
                },
            ))
            .expect("send turn completed");
        drop(events);
        drop(reporter);

        let output = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(output) = std::fs::read_to_string(&calls)
                    && output.lines().count() >= 6
                {
                    break output;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("lifecycle reports and release should finish");
        let lines: Vec<&str> = output.lines().collect();

        assert!(lines[0].contains("pane report-metadata w1:p2"));
        assert!(lines[0].contains("--display-agent Yolop · test"));
        assert!(lines[1].contains("--state idle"));
        assert!(lines[2].contains("pane report-metadata w1:p2"));
        assert!(lines[2].contains("--title Improve Herdr integration"));
        assert!(lines[2].contains("--display-agent Yolop · Improve Herdr integration"));
        assert!(lines[3].contains("--state working"));
        assert!(lines[4].contains("--state idle"));
        assert!(lines[5].contains("pane release-agent w1:p2"));
        let sequences: Vec<u64> = lines
            .iter()
            .map(|line| {
                let words: Vec<&str> = line.split_whitespace().collect();
                words
                    .windows(2)
                    .find_map(|pair| (pair[0] == "--seq").then_some(pair[1]))
                    .expect("sequence argument")
                    .parse()
                    .expect("numeric sequence")
            })
            .collect();
        assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn skill_content_is_conditional() {
        assert!(HerdrCapability::skill_content(false).is_none());
        let skill = HerdrCapability::skill_content(true).expect("active skill");
        assert!(skill.contains("name: herdr"));
        assert!(skill.contains("$HERDR_PANE_ID"));
    }
}

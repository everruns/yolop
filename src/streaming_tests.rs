//! End-to-end streaming tests.
//!
//! These tests drive a real `InProcessRuntime` (via `runtime::build`)
//! against the bundled llmsim driver, which emits real per-token chunks
//! through its `TokenStreamBuilder`. We then assert the full pipeline:
//!
//!   llmsim driver chunks → reason atom → JsonlEventEmitter::emit →
//!   broadcast channel → `app::handle_live_event` → `TurnEvent::Stream`
//!
//! The unit tests in `app.rs` cover the renderer in isolation with
//! synthetic events; these tests cover the wiring that turns those
//! synthetic assumptions into real behavior.

use std::collections::HashSet;
use std::time::Duration;

use everruns_core::events::EventData;
use tokio::sync::mpsc;

use everruns_core::llmsim_driver::LlmSimConfig;

use std::sync::Arc;

use crate::app::{DeltaRouter, StreamKind, TurnEvent, handle_live_event};
use crate::runtime::{BuildOptions, ProviderChoice, build_with_options};
use crate::settings::SettingsStore;

/// Maximum wall time we wait for the llmsim turn to fully drain
/// through the broadcast. The instant-latency llmsim profile finishes
/// in <100ms locally; the buffer is generous so a slow CI box won't
/// flake.
const TURN_TIMEOUT: Duration = Duration::from_secs(15);

async fn build_llmsim_runtime() -> crate::runtime::BuiltRuntime {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let sessions = tempfile::tempdir().expect("sessions tempdir");
    // tempdirs intentionally leak past the test body: the runtime
    // canonicalizes the workspace path, so we keep the handle alive
    // by std::mem::forget'ing it — these are tmpfs paths under the
    // OS-managed temp tree, so the OS cleans them.
    //
    // Use a lorem-200 response with simulate_latency enabled
    // (LatencyProfile::fast → ~1ms/token TBT). 200 tokens × 1ms ≈
    // 200ms wall-clock, which crosses the reason atom's 100ms delta
    // batch window twice. Without this the bundled fixed message
    // streams in <1ms and the batcher coalesces every chunk into a
    // single OutputMessageDelta.
    let llmsim = LlmSimConfig::lorem(200)
        .with_latency()
        .with_model("llmsim-yolop");
    let settings_path = sessions.path().join("settings.toml");
    let settings = Arc::new(SettingsStore::open(settings_path));
    let runtime = build_with_options(
        workspace.path().to_path_buf(),
        ProviderChoice::Sim,
        None,
        sessions.path().to_path_buf(),
        settings,
        BuildOptions {
            llmsim_override: Some(llmsim),
            ..BuildOptions::default()
        },
    )
    .await
    .expect("build llmsim runtime");
    std::mem::forget(workspace);
    std::mem::forget(sessions);
    runtime
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llmsim_emits_multiple_output_message_deltas_on_broadcast() {
    let runtime = build_llmsim_runtime().await;
    let handles = runtime.handles.clone();
    let mut live = handles.events.subscribe();

    let session_id = handles.session_id;
    let input = runtime.model.input_message("anything");
    let runtime_clone = handles.runtime.clone();
    let turn = tokio::spawn(async move { runtime_clone.run_turn(session_id, input).await });

    let mut deltas: Vec<String> = Vec::new();
    let mut got_completed = false;
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    while !got_completed && tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, live.recv()).await {
            Ok(Ok(event)) => match &event.data {
                EventData::OutputMessageDelta(d) => deltas.push(d.accumulated.clone()),
                EventData::OutputMessageCompleted(_) => got_completed = true,
                _ => {}
            },
            Ok(Err(_)) | Err(_) => break,
        }
    }
    turn.await.expect("turn join").expect("turn result");

    assert!(
        got_completed,
        "did not receive OutputMessageCompleted within {TURN_TIMEOUT:?}; deltas={deltas:?}"
    );
    assert!(
        deltas.len() >= 2,
        "expected ≥2 OutputMessageDelta events from llmsim's token stream, got {}: {:?}",
        deltas.len(),
        deltas
    );
    // Accumulated text grows monotonically: each delta carries the
    // running total, not just the new chunk. A regression here would
    // mean the broadcast is reordering or replaying events.
    for window in deltas.windows(2) {
        assert!(
            window[1].len() >= window[0].len(),
            "accumulated text shrank between deltas: {:?} -> {:?}",
            window[0],
            window[1],
        );
    }
    // The final accumulated text must be non-empty (llmsim has a fixed
    // multi-word response wired in runtime::build).
    let final_text = deltas.last().expect("at least one delta");
    assert!(
        !final_text.trim().is_empty(),
        "final accumulated text was empty: {deltas:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_deltas_drive_streaming_preview_through_handle_live_event() {
    // This is the meat of the test suite: take the actual events
    // produced by llmsim and walk them through the same routing code
    // the TUI uses. Asserts that a real provider's delta stream
    // produces growing `StreamPreview::Assistant` previews and a
    // clearing `Stream(None)` on completion.
    let runtime = build_llmsim_runtime().await;
    let handles = runtime.handles.clone();
    let mut live = handles.events.subscribe();

    let session_id = handles.session_id;
    let input = runtime.model.input_message("anything");
    let runtime_clone = handles.runtime.clone();
    let turn = tokio::spawn(async move { runtime_clone.run_turn(session_id, input).await });

    let (tx, mut rx) = mpsc::unbounded_channel::<TurnEvent>();
    let mut emitted = HashSet::new();
    let mut router = DeltaRouter::default();

    let mut saw_completed_event = false;
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    while !saw_completed_event && tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match tokio::time::timeout(remaining, live.recv()).await {
            Ok(Ok(event)) => {
                if event.session_id != session_id {
                    continue;
                }
                let is_completion = matches!(
                    event.data,
                    EventData::OutputMessageCompleted(_) | EventData::OutputMessageReplaced(_)
                );
                handle_live_event(&event, &mut emitted, &mut router, &tx);
                if is_completion {
                    saw_completed_event = true;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    turn.await.expect("turn join").expect("turn result");

    // Drain any remaining TurnEvents the receiver hasn't consumed yet.
    let mut previews: Vec<Option<crate::app::StreamPreview>> = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let TurnEvent::Stream(preview) = event {
            previews.push(preview);
        }
    }

    let assistant_previews: Vec<&crate::app::StreamPreview> = previews
        .iter()
        .filter_map(|p| p.as_ref())
        .filter(|p| p.kind == StreamKind::Assistant)
        .collect();
    assert!(
        assistant_previews.len() >= 2,
        "expected ≥2 assistant stream previews from the real llmsim stream, got {}: {:?}",
        assistant_previews.len(),
        previews
    );
    // Accumulated preview text must grow monotonically — same
    // invariant as the raw delta accumulator, but observed through the
    // exact code path the TUI uses to render.
    for window in assistant_previews.windows(2) {
        assert!(
            window[1].text.len() >= window[0].text.len(),
            "preview text shrank between deltas: {:?} -> {:?}",
            window[0].text,
            window[1].text,
        );
    }
    // The completion event must clear the preview at least once.
    assert!(
        previews.iter().any(|p| p.is_none()),
        "expected Stream(None) to clear preview after completion; got {previews:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broadcast_does_not_persist_delta_events_to_jsonl() {
    // Cross-check the persistence claim from session_log.rs comments:
    // delta events fan out to live subscribers but never hit disk.
    // A regression here would balloon the on-disk session log O(n²).
    // Subscribe before the turn so we can prove the broadcast did
    // carry deltas — otherwise the "not on disk" assertion is
    // vacuously true (no deltas to leak either way).
    let runtime = build_llmsim_runtime().await;
    let log_path = runtime.startup.session_log_path.clone();
    let mut live = runtime.handles.events.subscribe();

    let session_id = runtime.handles.session_id;
    let input = runtime.model.input_message("anything");
    runtime
        .handles
        .runtime
        .run_turn(session_id, input)
        .await
        .expect("turn");

    let mut saw_delta = false;
    while let Ok(event) = live.try_recv() {
        if matches!(event.data, EventData::OutputMessageDelta(_)) {
            saw_delta = true;
        }
    }
    assert!(
        saw_delta,
        "test setup expected at least one OutputMessageDelta on the broadcast",
    );

    let on_disk = std::fs::read_to_string(&log_path).expect("read session log");
    assert!(
        !on_disk.contains("\"type\":\"output.message.delta\""),
        "JSONL log must not persist delta events: {on_disk}"
    );
    assert!(
        !on_disk.contains("\"type\":\"tool.output.delta\""),
        "JSONL log must not persist tool output delta events: {on_disk}"
    );
}

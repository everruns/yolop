//! `yolop-extension-cache-break` — a YEP capability server that watches the
//! session's `llm.generation` events and warns when prompt-cache reuse
//! collapses between turns.
//!
//! A cache break is invisible in the transcript and expensive: the prefix that
//! was being reused is billed as fresh input for the rest of the session. The
//! counters that reveal it already flow past — `trace` subscribes to them,
//! `status` puts the warning where the user is looking.
//!
//! Two facets, no tools: the extension observes and reports, and never touches
//! the agent loop. `trace/event` is fire-and-forget, so nothing here can stall
//! a turn.

mod detector;

use detector::{DEFAULT_DROP_TOKENS, Detector};
use std::sync::{Arc, Mutex};
use yolop_yep::protocol::{StatusChangedParams, StatusLevel};
use yolop_yep::{Notifier, Server};

fn main() -> std::io::Result<()> {
    // The notifier is built first: handlers are moved into the builder, so they
    // have to capture their sender before the server exists.
    let notifier = Notifier::to_stdout();

    // Shared because two handlers need it: `on_config` rebuilds it with the
    // configured threshold, `on_trace` feeds it. The serve loop is
    // single-threaded, so the lock is never contended.
    let detector = Arc::new(Mutex::new(Detector::new(DEFAULT_DROP_TOKENS)));

    let configured = (detector.clone(), notifier.clone());
    let observed = (detector, notifier);

    Server::new("cache-break")
        .on_config(move |config| {
            let (detector, notifier) = &configured;
            // `on_config` runs at handshake, before any trace event — early
            // enough that no sample is taken at the default threshold.
            if let Some(tokens) = config.get("drop_tokens").and_then(|v| v.as_u64()) {
                *detector.lock().expect("detector lock") = Detector::new(tokens);
            }
            // Clear any status left over from a previous run of this server
            // (a `reload_extension` respawns it mid-session).
            notifier.status(&StatusChangedParams::default());
        })
        .on_trace(move |event| {
            let (detector, notifier) = &observed;
            let notice = detector.lock().expect("detector lock").observe(event);
            let Some(notice) = notice else {
                return;
            };
            notifier.status(&StatusChangedParams {
                status: notice.status(),
                level: if notice.is_warning() {
                    StatusLevel::Warn
                } else {
                    StatusLevel::Ok
                },
                detail: Some(notice.detail()),
            });
            // `--print`/ACP have no status bar, so the log line is the whole
            // report there; in the TUI it is the durable record of a warning
            // the status bar will later clear.
            if notice.is_warning() {
                notifier.log(notice.detail());
            }
        })
        .serve()
}

//! `yolop-extension-logfire` — a yolop extension capability server that exports
//! the agent's live trace to [Pydantic Logfire](https://logfire.pydantic.dev/).
//!
//! It declares the `trace` facet: yolop forwards each agentic-lifecycle event
//! (`turn.*`, `reason.*`, `act.*`, `tool.*`, `llm.generation`) as a
//! fire-and-forget `trace/event` notification. This server folds those into
//! OpenTelemetry spans ([`exporter`]) and POSTs them to Logfire's OTLP/HTTP
//! endpoint — the same wire Logfire's own SDKs use, so runs show up as nested
//! traces with no Logfire library dependency.
//!
//! Configuration is by environment (mirroring Logfire's onboarding checklist,
//! <https://logfire.pydantic.dev/docs/>), inherited from the yolop process:
//!
//! - `LOGFIRE_TOKEN` (or `LOGFIRE_WRITE_TOKEN`) — the write token. **Required**;
//!   without it spans are built but dropped (a one-line stderr note), so the
//!   extension is inert until configured.
//! - `LOGFIRE_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) — the OTLP
//!   traces URL. Defaults to Logfire's ingest endpoint.
//! - `OTEL_SERVICE_NAME` (or `LOGFIRE_SERVICE_NAME`) — service name on the
//!   spans. Defaults to `yolop`.

mod exporter;

use exporter::SpanExporter;
use std::io::Write;
use yolop_yep::Server;

const DEFAULT_ENDPOINT: &str = "https://logfire-api.pydantic.dev/v1/traces";
const DEFAULT_SERVICE: &str = "yolop";

fn env_any(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

fn main() -> std::io::Result<()> {
    let token = env_any(&["LOGFIRE_TOKEN", "LOGFIRE_WRITE_TOKEN"]);
    let endpoint = env_any(&["LOGFIRE_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"])
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
    let service = env_any(&["OTEL_SERVICE_NAME", "LOGFIRE_SERVICE_NAME"])
        .unwrap_or_else(|| DEFAULT_SERVICE.to_string());

    if token.is_none() {
        // stderr is the server's free log channel (yolop drains it to tracing).
        let _ = writeln!(
            std::io::stderr(),
            "yolop-extension-logfire: no LOGFIRE_TOKEN set; traces will be dropped. \
             Set LOGFIRE_TOKEN to export to Logfire."
        );
    }

    let http = build_http_sink(token, endpoint);
    let mut exporter = SpanExporter::new(service, http);

    Server::new("logfire")
        .on_trace(move |event| exporter.handle(event))
        .serve()
}

/// Build the OTLP/HTTP sink: POST each serialized batch to Logfire. Blocking
/// (the serve loop is single-threaded), agent reused across calls. All failures
/// degrade to a stderr note — the exporter must never sink the agent.
fn build_http_sink(token: Option<String>, endpoint: String) -> impl FnMut(&str) {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    move |body: &str| {
        let Some(token) = token.as_deref() else {
            return; // no token: already warned at startup; drop silently
        };
        let result = agent
            .post(&endpoint)
            .set("Content-Type", "application/json")
            // Logfire authenticates OTLP with the write token in `Authorization`.
            .set("Authorization", token)
            .send_string(body);
        if let Err(err) = result {
            let _ = writeln!(
                std::io::stderr(),
                "yolop-extension-logfire: export failed: {err}"
            );
        }
    }
}

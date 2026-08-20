//! `yolop-extension-logfire` — a yolop extension capability server that exports
//! the agent's live trace to [Pydantic Logfire](https://logfire.pydantic.dev/).
//!
//! It declares the `trace` facet: yolop forwards each agentic-lifecycle event
//! (`turn.*`, `reason.*`, `act.*`, `tool.*`, `llm.generation`) as a
//! fire-and-forget `trace/event` notification. This server folds those into
//! OpenTelemetry spans ([`exporter`]) and ships them to Logfire through the
//! official OTLP/HTTP exporter — the approach Logfire's "alternative clients"
//! guide documents for Rust, so no Logfire client library is needed.
//!
//! Configuration is by environment (inherited from the yolop process), matching
//! Logfire's onboarding checklist, <https://logfire.pydantic.dev/docs/>:
//!
//! - `LOGFIRE_TOKEN` (or `LOGFIRE_WRITE_TOKEN`) — the write token, sent raw in
//!   the `Authorization` header (no `Bearer` prefix, per Logfire's OTLP setup).
//!   **Required**; without it the server runs but exports nowhere (a one-line
//!   stderr note), so the extension is inert until configured.
//! - `LOGFIRE_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) — the OTLP
//!   traces URL. Defaults to Logfire's US ingest endpoint.
//! - `OTEL_SERVICE_NAME` (or `LOGFIRE_SERVICE_NAME`) — service name on the
//!   spans. Defaults to `yolop`.

mod exporter;
mod gen_ai;

use exporter::TraceExporter;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

const DEFAULT_ENDPOINT: &str = "https://logfire-us.pydantic.dev/v1/traces";
const DEFAULT_SERVICE: &str = "yolop";
const TRACER_NAME: &str = "yolop-extension-logfire";

fn env_any(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

fn config_str(config: &Value, key: &str, env_keys: &[&str], default: &str) -> String {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| env_any(env_keys))
        .unwrap_or_else(|| default.to_string())
}

fn main() -> std::io::Result<()> {
    // The token is a `secret` config field, injected by the host as env; never
    // delivered via `initialize.config`.
    let token = env_any(&["LOGFIRE_TOKEN", "LOGFIRE_WRITE_TOKEN"]);
    if token.is_none() {
        // stderr is the server's free log channel (yolop drains it to tracing).
        let _ = writeln!(
            std::io::stderr(),
            "yolop-extension-logfire: no LOGFIRE_TOKEN set; traces will not be exported. \
             Set LOGFIRE_TOKEN to send to Logfire."
        );
    }

    // Built from `initialize.config` (endpoint/service_name), which arrives
    // before any trace event; shared with the trace handler via a mutex (the
    // serve loop is single-threaded, so it's uncontended).
    let exporter: Arc<Mutex<Option<TraceExporter>>> = Arc::new(Mutex::new(None));
    let on_config_exporter = exporter.clone();
    let on_trace_exporter = exporter.clone();

    yolop_yep::Server::new("logfire")
        .on_config(move |config| {
            let endpoint = config_str(
                config,
                "endpoint",
                &["LOGFIRE_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"],
                DEFAULT_ENDPOINT,
            );
            let service = config_str(
                config,
                "service_name",
                &["OTEL_SERVICE_NAME", "LOGFIRE_SERVICE_NAME"],
                DEFAULT_SERVICE,
            );
            let provider = build_provider(token.clone(), endpoint, service);
            let tracer = provider.tracer(TRACER_NAME);
            *on_config_exporter.lock().unwrap() = Some(TraceExporter::new(tracer, provider));
        })
        .on_trace(move |event| {
            if let Some(exp) = on_trace_exporter.lock().unwrap().as_mut() {
                exp.handle(event);
            }
        })
        .serve()
}

/// Build the tracer provider. With a token, a simple (synchronous, blocking)
/// OTLP/HTTP exporter POSTs each span to Logfire as it ends — fitting the
/// single-threaded serve loop without a tokio runtime. Without a token, the
/// provider has no exporter, so spans are built and dropped (inert).
fn build_provider(token: Option<String>, endpoint: String, service: String) -> SdkTracerProvider {
    let resource = Resource::builder().with_service_name(service).build();
    let Some(token) = token else {
        return SdkTracerProvider::builder().with_resource(resource).build();
    };

    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), token);

    match SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .with_headers(headers)
        .build()
    {
        Ok(exporter) => SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .with_resource(resource)
            .build(),
        Err(err) => {
            let _ = writeln!(
                std::io::stderr(),
                "yolop-extension-logfire: could not build OTLP exporter ({err}); \
                 traces will not be exported"
            );
            SdkTracerProvider::builder().with_resource(resource).build()
        }
    }
}

# yolop-extension-logfire

A [yolop](https://github.com/everruns/yolop) extension that exports the agent's
live trace — turns, reasoning, tool calls, and LLM generations — to
[Pydantic Logfire](https://logfire.pydantic.dev/) so a run shows up as a nested
trace with token usage, timing, and tool arguments.

It uses yolop's `trace` extension facet: the host forwards each agentic
lifecycle event as a fire-and-forget notification, and this server folds them
into OpenTelemetry spans and POSTs them to Logfire's OTLP/HTTP endpoint. No
Logfire client library is required — it speaks OTLP directly, the same wire
Logfire's own SDKs use.

## Install

```bash
# From this repo (dev): build the binary onto PATH, then install the package.
cargo install --path crates/yolop-extension-logfire
/extensions install <path-to>/crates/yolop-extension-logfire
/extensions enable logfire
```

The server binary (`yolop-extension-logfire`) must be resolvable on `PATH` or in
the package's `bin/`, per the usual extension rule.

## Configure

Configuration is by environment, inherited from the yolop process — the same
variables Logfire's onboarding checklist uses:

| Variable | Purpose | Default |
| --- | --- | --- |
| `LOGFIRE_TOKEN` (or `LOGFIRE_WRITE_TOKEN`) | Logfire **write token**. Required — without it spans are dropped and the extension is inert. | — |
| `LOGFIRE_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) | OTLP traces URL | `https://logfire-api.pydantic.dev/v1/traces` |
| `OTEL_SERVICE_NAME` (or `LOGFIRE_SERVICE_NAME`) | Service name on the spans | `yolop` |

```bash
export LOGFIRE_TOKEN=pylf_v1_...   # from your Logfire project settings
doppler run -- cargo run -- -p "say hi"   # this run is now traced to Logfire
```

## What gets exported

One trace per session; each turn is a root span, with `reason`/`act`/`tool`
spans nested under their turn. Event `data` scalars (token counts, model, tool
name, …) become span attributes; `*.failed` events mark the span's status as
error. High-frequency streaming deltas are not exported — the paired
`*.started` / `*.completed` events already bound the work.

Export is best-effort: a slow or unreachable Logfire never stalls the agent, and
spans flush at each turn boundary.

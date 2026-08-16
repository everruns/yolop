# yolop-extension-logfire

A [yolop](https://github.com/everruns/yolop) extension that exports the agent's
live trace, turns, reasoning, tool calls, and LLM generations, to
[Pydantic Logfire](https://logfire.pydantic.dev/) so a run shows up as a nested
trace with token usage, timing, and tool arguments.

It uses yolop's `trace` extension facet: the host forwards each agentic
lifecycle event as a fire-and-forget notification, and this server folds them
into OpenTelemetry spans and ships them to Logfire through the official
`opentelemetry-otlp` exporter (HTTP/protobuf, blocking client), the approach
Logfire's [alternative-clients guide](https://logfire.pydantic.dev/docs/how-to-guides/alternative-clients/)
documents for Rust. No Logfire client library is required.

## Install

```bash
# Published package (installs the binary and extension manifest together).
cargo install yolop-extension-logfire
/extensions install crates.io:yolop-extension-logfire
/extensions enable logfire

# From this repo (development checkout).
cargo install --path extensions/yolop-extension-logfire
/extensions install <path-to>/extensions/yolop-extension-logfire
/extensions enable logfire
```

The server binary (`yolop-extension-logfire`) must be resolvable on `PATH` or in
the package's `bin/`, per the usual extension rule.

## Configure

The token is a `secret` config field, so on `enable_extension` (or via
`set_extension_secret name=logfire field=token`) yolop prompts **you** for it
directly, with masked input, the agent never sees the value. It's stored in the
credential store (`connections.toml`, 0600), not in settings, and injected into
this server as `LOGFIRE_TOKEN`. `endpoint` and `service_name` are ordinary
config fields, read from `initialize.config`.

You can also set everything by environment, inherited from the yolop process,
the same variables Logfire's onboarding checklist uses (env always overrides
stored config):

| Variable | Purpose | Default |
| --- | --- | --- |
| `LOGFIRE_TOKEN` (or `LOGFIRE_WRITE_TOKEN`) | Logfire **write token**, sent raw in the `Authorization` header (no `Bearer` prefix). Required, without it the extension runs but exports nowhere. |, |
| `LOGFIRE_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) | OTLP traces URL. Use `logfire-eu` for an EU project. | `https://logfire-us.pydantic.dev/v1/traces` |
| `OTEL_SERVICE_NAME` (or `LOGFIRE_SERVICE_NAME`) | Service name on the spans | `yolop` |

```bash
export LOGFIRE_TOKEN=pylf_v1_...   # from your Logfire project settings
doppler run -- cargo run -- -p "say hi"   # this run is now traced to Logfire
```

## What gets exported

One trace per turn: `turn.*` is the root span, with `reason`/`act`/`tool` spans
nested under it and `llm.generation` as a point span. Event `data` scalars
(token counts, model, tool name, exit code, …) become span attributes;
`*.failed` events mark the span's status as error. High-frequency streaming
deltas are not exported, the paired `*.started` / `*.completed` events already
bound the work.

Export is best-effort: a slow or unreachable Logfire never stalls the agent (the
host never awaits `trace/event`), and spans are exported as they end.

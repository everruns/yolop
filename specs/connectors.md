# Connectors and remote sandboxes

Status: v1 implemented (Daytona).

## Why

Yolop runs unsandboxed on the user's host by default. Some tasks — untrusted
code, heavy builds, network experiments — should run in an isolated remote
environment instead. [Daytona](https://www.daytona.io/) provides cloud Linux
sandboxes; upstream ships the integration as `everruns-integrations-daytona`.

Connectors are yolop's generic credential layer for those backends. The same
interface can register additional sandbox providers (E2B, etc.) without
rewriting host wiring.

## What

### Connector storage

Credentials persist in `<config_dir>/yolop/connections.toml`, beside
`settings.toml`. Values are owner-only on Unix. Environment variables remain
supported as overrides (`DAYTONA_API_KEY` for Daytona).

### Connector catalog

`src/connectors/catalog.rs` registers upstream [`ConnectionProvider`]
implementations. Daytona is registered by default; new providers add a
`.register(...)` call there.

### Runtime wiring

- **`connectors`** is enabled on the default harness — always available
  for listing providers and saving credentials.
- **`daytona`** and **`session_storage`** are registered but **not** on the
  default harness. Opt in through the generic capability config in
  `settings.toml` (see [`configuration.md`](./configuration.md)).
- `YolopConnectionResolver` implements `UserConnectionResolver` and is injected
  through `RuntimeBackends::with_connection_resolver`.
- `DaytonaConnectionProvider` is registered on `PlatformDefinition` for form
  schema and validation.

When `daytona` is enabled via `[[capabilities]]`, yolop automatically adds
`session_storage` to the harness (Daytona's upstream dependency).

### Enabling Daytona

```toml
[[capabilities]]
ref = "daytona"
enabled = true
```

Optional direct API calling:

```toml
[[capabilities]]
ref = "daytona"
enabled = true
enable_api_calling = true
```

Inspect the registered catalog with `get_config key=capabilities` or
`get_config key=capabilities.daytona`.

### Tools (`connectors` capability, default on)

| Tool | Purpose |
|------|---------|
| `list_connectors` | Available providers + connected status |
| `get_connector` | One provider's instructions and form schema |
| `connect` | Validate and save credentials |
| `disconnect` | Remove stored credentials |

### Daytona tools (upstream `daytona` capability, opt-in)

When enabled and connected, the agent can create sandboxes and outsource work
remotely: `daytona_create_sandbox`, `daytona_exec`, file tools, git helpers,
and lifecycle management. See upstream `everruns-integrations-daytona` / Everruns
docs for the full tool reference.

## Boundaries

- Connectors store integration credentials, not LLM provider tokens (those stay
  in `settings.toml` / `/setup token`).
- Host `bash` and file tools still target the local workspace; Daytona is opt-in
  per task via `daytona_*` tools after enabling the capability.
- OAuth connectors are listed but not yet configurable through `connect`.

## See also

- [`specs/configuration.md`](./configuration.md) — `[[capabilities]]` harness overrides
- [`specs/maintenance.md`](./maintenance.md) — host threat surface
- [Everruns Daytona integration](https://docs.everruns.com/integrations/daytona/)

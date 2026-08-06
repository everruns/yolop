---
type: Product Specification
title: Configuration schema
description: Defines the configuration schema contract for Yolop.
---

# Configuration schema

Status: v1 implemented.

## Why

yolop's global settings live in `settings.toml` in the platform config dir.
Optional named profiles live below `profiles/` and form a sparse execution
layer above global settings. Global loading is deliberately tolerant — unknown keys are ignored, never fatal
(see `Settings::from_table`) — so a user or another tool can add keys without
breaking yolop. The cost of that tolerance is that the file carries no
*semantics*: nothing tells the agent (or the user) what a key means, what type
it takes, what its default is, or how to set it safely.

The configuration schema fills that gap. It is an **informational** schema: it
never makes loading stricter, it adds meaning. That meaning is what lets the
agent edit yolop's own configuration the way a user describes it ("use anthropic
by default", "store my OpenAI key", "point at my local endpoint") instead of
forcing slash-command syntax.

## What

### Schema registry — single source of truth

`src/config/schema.rs` is a compile-time registry of `ConfigField`s. Each field
carries a canonical `key`, `aliases`, `title`, `description`, value `kind`
(`text` / `bool` / `secret`), effective `default`, `examples`, and whether it is
`provider_scoped` (addressed as `<key>.<provider>`). This one registry feeds
every configuration surface — the tools below and the `yolop-config` skill — so
there is no second place to keep in sync.

Keys are addressed the way a human would name them:

| Key                       | Type   | Meaning                                                        |
|---------------------------|--------|----------------------------------------------------------------|
| `default_provider`        | text   | Provider used when no `--provider` flag is given; takes precedence over env auto-detection. |
| `default_model`           | text   | Global fallback model spec for the active provider when no per-provider pick exists; only applied when the id is recognized for that provider. |
| `models.<provider>`       | text   | Per-provider model spec, survives provider switches.           |
| `tokens.<provider>`       | secret | Provider API token (owner-only on disk; env vars override).    |
| `base_urls.<provider>`    | text   | Endpoint base URL (used by the `custom` provider).             |
| `approval_mode`           | text   | Soft-approval paranoia level (`protective` / `normal` / `off`). |
| `approval_policy`         | text   | Hard shell approval policy (`untrusted` / `on-failure` / `on-request` / `never`). |
| `attribution`             | bool   | Commit/PR attribution on/off.                                  |
| `proactive_wake`          | bool   | Auto-start a turn when a background task finishes (TUI); on by default. |
| `worktrees`               | text   | Worktree isolation (`auto` / `always` / `off`).             |
| `sandbox_mode`            | text   | Shell containment (`read-only` / `workspace-write` / `danger-full-access`). |
| `capabilities`            | list   | Ordered `[[capabilities]]` harness overrides; `capabilities.<ref>` for schema metadata. |

`default_provider` is persisted under that name on disk; the legacy `provider`
key is still read (and accepted as an alias) so pre-rename settings files keep
working. `default_model` is applied as a cross-provider fallback in
`ProviderChoice::resolve_for_settings`, but only when the model id is
recognized for the active provider. At startup and on `/setup provider`
switches, yolop may also query the provider's models API when credentials
exist. Before a turn is checkpointed or sent, Yolop validates the selected model
once per process against that API when it is available. Unavailable models fail
without persisting the ask, so the user can select an advertised model and
submit the same turn. Providers without discovery support (including custom
compatible endpoints) continue without preflight.

### Named execution profiles

`--profile <name>` loads
`<config_dir>/yolop/profiles/<name>.toml`. Profile selection is explicit for
each process; it is never persisted and has no environment-variable selector.
Names contain 1–64 lowercase ASCII letters, numbers, hyphens, or underscores,
start with a letter or number, and are validated before constructing the path.

Profiles are sparse overlays. Scalars replace the global value and provider
maps merge by provider key. Resolution order is CLI or ACP live selection,
selected profile, global settings, and finally credential auto-detection or
built-in defaults. Provider-specific environment variables keep their existing
precedence where read (credentials and `CUSTOM_BASE_URL`). ACP's standard
`session/set_config_option` model and reasoning changes remain local to that ACP
session.

The profileable v1 keys are `default_provider`, `default_model`, `models`,
`base_urls`, `approval_mode`, `approval_policy`, `sandbox_mode`, and
`worktrees`. Credentials (`tokens`, `codex_auth`) and structural or personal
settings (`mcp`, `capabilities`, `theme`, `attribution`, `proactive_wake`) are
global-only and make a selected profile fail validation. Invalid known values
also fail startup; unknown keys produce a warning and are ignored for forward
compatibility.

`SettingsStore` retains the global document and sparse profile separately and
only returns their merged effective `Settings` snapshot. It never serializes
that merged snapshot into a profile, so inherited credentials cannot leak into
the profile file. Profileable writes from `/setup`, ACP mode selection, and
`set_config` target the active profile; global-only writes always target
`settings.toml`. Clearing a profile override removes it and reveals the global
value.

The active profile is printed at startup, returned by `get_config`, and
included in the terminal-independent safety status. Missing or malformed
profiles fail before session construction.

### Tools

The `config` capability (`src/capabilities/config.rs`) exposes two tools backed
by the schema:

- **`get_config`** — with no argument, returns every key with its semantics and
  current value; with a `key`, returns just that entry. Secrets are reported as
  `stored` / `unset`, never echoed. Use `key=capabilities` for the registered
  catalog plus stored overrides and effective harness, or `key=capabilities.<ref>`
  for one capability's schema metadata (`config_schema`, `config_ui_schema`).
- **`set_config`** — validates and persists scalar keys via `value` (`clear`
  unsets). Harness overrides: `key=capabilities` with a `json` object appends one
  `[[capabilities]]` entry; `value=clear` drops all stored overrides. Capability
  config is validated through each capability's `validate_config`.

Both honor aliases and validate provider segments against the supported-provider
list. Provider/model and capability edits take effect on the next run; `/setup`
remains the way to switch the *live* model mid-session.

### Configuration as a service

Configuration is exposed to the rest of the agent as a **service** so that
capabilities can read it without re-parsing the TOML or reaching into store
internals. `src/config/service.rs` defines the `ConfigService` trait:

- a generic `current(key)` that reads any value by its schema key (e.g.
  `models.openai`), with secrets reduced to `stored`/`unset`, and
- the two semantic getters that have dedicated consumers
  (`attribution_enabled`, `approval_mode`).

The surface is kept minimal: `current(key)` covers arbitrary reads, so a
capability adds a typed getter only when it grows a real need rather than
carrying speculative methods.

`SettingsStore` implements `ConfigService`, so the shared layered handle that
backs writes also serves effective reads. Read-only capabilities take only an
`Arc<dyn ConfigService>`; write-coupled capabilities hold both — reads go
through the service handle, writes through the concrete `SettingsStore` — so the
read/write split is explicit at the type level. `AttributionCapability` reads
whether attribution is enabled through the service; `ApprovalCapability` reads
its soft-approval paranoia level through `ConfigService::approval_mode()` each
turn; the `config` capability's `get_config` reads single values through
`ConfigService::current`; and `ModelsCapability` reads provider/token/model state
through its config handle while persisting `/setup` changes through the store.
`approval_mode` is also a first-class schema key, so `get_config`/`set_config`
manage it alongside everything else.

The per-target read helpers (`current_value`, `scoped_current`) live in the
service module so the `config` tools and any other consumer share one
implementation; secret redaction and unsupported-provider filtering therefore
apply uniformly wherever config is read.

### Context delivery

The schema reaches the agent two ways:

1. An always-on pointer: `ConfigCapability::system_prompt_contribution` adds a
   compact note (settings path + "use `get_config`/`set_config` or the
   `yolop-config` skill") to every turn.
2. The bundled `yolop-config` system skill is the
   detailed, on-demand reference. It instructs the agent to read the live schema
   via `get_config` rather than duplicating key lists, and points at the
   adjacent surfaces (`memory`, `yolop-hooks`, `/setup`).

## Boundaries

Configuration is distinct from the neighbouring personalization surfaces:
durable preferences are **memory**, behavioral rules are **hooks**, and
interactive live provider/model switching is **`/setup`**. All settings keys,
including harness capabilities, go through `get_config` / `set_config`.

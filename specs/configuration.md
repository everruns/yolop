# Configuration schema

Status: v1 implemented.

## Why

yolop's settings live in one TOML file (`settings.toml` in the platform config
dir). Loading is deliberately tolerant — unknown keys are ignored, never fatal
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

`src/config_schema.rs` is a compile-time registry of `ConfigField`s. Each field
carries a canonical `key`, `aliases`, `title`, `description`, value `kind`
(`text` / `bool` / `secret`), effective `default`, `examples`, and whether it is
`provider_scoped` (addressed as `<key>.<provider>`). This one registry feeds
every configuration surface — the tools below and the `yolop-config` skill — so
there is no second place to keep in sync.

Keys are addressed the way a human would name them:

| Key                       | Type   | Meaning                                                        |
|---------------------------|--------|----------------------------------------------------------------|
| `default_provider`        | text   | Provider used when no `--provider` flag is given; takes precedence over env auto-detection. |
| `default_model`           | text   | Global fallback model spec for the active provider; a per-provider pick wins over it. |
| `models.<provider>`       | text   | Per-provider model spec, survives provider switches.           |
| `tokens.<provider>`       | secret | Provider API token (owner-only on disk; env vars override).    |
| `base_urls.<provider>`    | text   | Endpoint base URL (used by the `custom` provider).             |
| `approval_mode`           | text   | Soft-approval paranoia level (`protective` / `normal` / `off`). |
| `attribution`             | bool   | Commit/PR attribution on/off.                                  |
| `capabilities.<name>`     | bool   | Optional-capability toggle; `clear` restores the catalog default (see `specs/capabilities.md`). |

`default_provider` is persisted under that name on disk; the legacy `provider`
key is still read (and accepted as an alias) so pre-rename settings files keep
working. `default_model` is applied as a cross-provider fallback in
`ProviderChoice::with_saved_model`.

Scoped keys carry a `KeyScope` that says what the dotted segment is validated
against: `Provider` keys (`tokens`, `models`, `base_urls`) against the
supported-provider list, `Capability` keys (`capabilities`) against the
optional-capability catalog in `src/capabilities/optional.rs`.

### Tools

The `config` capability (`src/capabilities/config.rs`) exposes two tools backed
by the schema:

- **`get_config`** — with no argument, returns every key with its semantics and
  current value; with a `key`, returns just that entry. Secrets are reported as
  `stored` / `unset`, never echoed.
- **`set_config`** — validates a `key`/`value` against the schema and persists
  through `SettingsStore` (atomic write, owner-only). `value=clear` unsets an
  optional or secret key.

Both honor aliases and validate provider segments against the supported-provider
list. Provider/model edits take effect on the next run; `/setup` remains the way
to switch the *live* model mid-session.

### Configuration as a service

Configuration is exposed to the rest of the agent as a **service** so that
capabilities can read it without re-parsing the TOML or reaching into store
internals. `src/config_service.rs` defines the `ConfigService` trait:

- a generic `current(key)` that reads any value by its schema key (e.g.
  `models.openai`), with secrets reduced to `stored`/`unset`, and
- the two semantic getters that have dedicated consumers
  (`attribution_enabled`, `approval_mode`).

The surface is kept minimal: `current(key)` covers arbitrary reads, so a
capability adds a typed getter only when it grows a real need rather than
carrying speculative methods.

`SettingsStore` implements `ConfigService`, so the single shared handle that
backs writes also serves reads. Read-only capabilities take only an
`Arc<dyn ConfigService>`; write-coupled capabilities hold both — reads go
through the service handle, writes through the concrete `SettingsStore` — so the
read/write split is explicit at the type level. `AttributionCapability` reads
whether attribution is enabled through the service; `ApprovalCapability` reads
its soft-approval paranoia level through `ConfigService::approval_mode()` each
turn; the `config` capability's `get_config` reads single values through
`ConfigService::current`; and `SetupCapability` reads provider/token/model state
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
2. The `yolop-config` built-in skill (`skills/yolop-config/SKILL.md`) is the
   detailed, on-demand reference. It instructs the agent to read the live schema
   via `get_config` rather than duplicating key lists, and points at the
   adjacent surfaces (`your` memory, `yolop-hooks`, `/setup`).

## Boundaries

Configuration is distinct from the neighbouring personalization surfaces:
durable preferences are `your` **memory**, behavioral rules are **hooks**, and
interactive live provider/model switching is **`/setup`**. `set_config` is only
for the typed settings keys above.

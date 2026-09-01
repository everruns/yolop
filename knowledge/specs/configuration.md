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
layer above global settings. Global loading is deliberately tolerant, unknown keys are ignored, never fatal
(see `Settings::from_table`), so a user or another tool can add keys without
breaking yolop. The cost of that tolerance is that the file carries no
*semantics*: nothing tells the agent (or the user) what a key means, what type
it takes, what its default is, or how to set it safely.

The configuration schema fills that gap. It is an **informational** schema: it
never makes loading stricter, it adds meaning. That meaning is what lets the
agent edit yolop's own configuration the way a user describes it ("use anthropic
by default", "store my OpenAI key", "point at my local endpoint") instead of
forcing slash-command syntax.

## What

### Schema registry, single source of truth

`src/config/schema.rs` is a compile-time registry of `ConfigField`s. Each field
carries a canonical `key`, `aliases`, `title`, `description`, value `kind`
(`text` / `bool` / `secret`), effective `default`, `examples`, and whether it is
`provider_scoped` (addressed as `<key>.<provider>`). This one registry feeds
every configuration surface and the `yolop-config` skill, so
there is no second place to keep in sync.

Keys are addressed the way a human would name them:

| Key                       | Type   | Meaning                                                        |
|---------------------------|--------|----------------------------------------------------------------|
| `default_provider`        | text   | Provider used when no `--provider` flag is given; takes precedence over env auto-detection. |
| `models`                  | list   | The ordered, cross-provider `[[models]]` menu `/model` and ACP offer; edited with `yolop config models` (see [Model list](model-list.md)). |
| `default_models.<provider>` | text | Per-provider model spec, survives provider switches. Remembered state, not the menu. |
| `tokens.<provider>`       | secret | Provider API token (owner-only on disk; env vars override). OpenRouter PKCE browser login stores the minted key as `tokens.openrouter`. |
| `base_urls.<provider>`    | text   | Endpoint base URL (used by the `custom` provider).             |
| `approval_mode`           | text   | Soft-approval paranoia level (`protective` / `normal` / `off`). |
| `approval_policy`         | text   | Hard shell approval policy (`untrusted` / `on-failure` / `on-request` / `never`). |
| `attribution`             | bool   | Commit/PR attribution on/off.                                  |
| `proactive_wake`          | bool   | Auto-start a turn when a background task finishes (TUI); on by default. |
| `acp_setup_page`          | bool   | Offer the loopback provider setup page to ACP clients; off by default (see [ACP](acp.md)). |
| `worktrees`               | text   | Worktree isolation (`auto` / `always` / `off`).             |
| `sandbox_mode`            | text   | Shell containment (`read-only` / `workspace-write` / `danger-full-access`). |
| `capabilities`            | list   | Ordered `[[capabilities]]` harness overrides; `capabilities.<ref>` for schema metadata. |

`default_provider` is persisted under that name on disk; the legacy `provider`
key is still read (and accepted as an alias) so pre-rename settings files keep
working. Model preferences are provider-scoped; a disconnected provider's
preference is never treated as a usable active model. At startup and on `/setup provider`
switches, yolop may also query the provider's models API when credentials
exist. Before a turn is checkpointed or sent, Yolop validates the selected model
once per process against that API when it is available. Unavailable models fail
without persisting the ask, so the user can select an advertised model and
submit the same turn. Providers without discovery support (including custom
compatible endpoints) continue without preflight.

Meta Model API is a first-class `meta` provider backed by `everruns-meta`, not a
generic compatible endpoint. It reads `MODEL_API_KEY`, defaults to
`muse-spark-1.2`, and exposes both the standard and
`muse-spark-1.2-contributor` published profiles.

### Top-level config command

`yolop config` is the persistent configuration surface in both attached and
detached execution. `get [key]` returns the whole schema-backed view or one
field, `set <key> <value>` applies scalar and provider-scoped values, and
`clear <key>` removes a stored value. Structured capability overrides use
`set capabilities --json '<object>'`; this keeps object input distinct from
scalar values and appends through the same schema and settings service used by
the other forms. `clear capabilities` removes all stored overrides.

The same command also owns `model show|set|clear` and the `models` list editor.
It replaces the former `get_config` and `set_config` agent tools, which are no
longer registered.

### Where the global directories live

Every global path derives from one of two roots, both owned by
`src/config/paths.rs`:

| Root   | Default                     | Holds                                                        |
|--------|-----------------------------|--------------------------------------------------------------|
| config | `<config_dir>/yolop/`       | `settings.toml`, `profiles/`, `hooks.json`, `extensions/`     |
| data   | `<data_dir>/yolop/`         | `sessions/`, `logs/`, `models/`, `prompt_history.jsonl`, materialized system skills, `crashes/` |

`<config_dir>` and `<data_dir>` are the platform directories (`~/.config` and
`~/.local/share` on Linux, `~/Library/Application Support` on macOS, `%APPDATA%`
on Windows). Either root can be moved with `--config-dir` / `--data-dir` or
`YOLOP_CONFIG_DIR` / `YOLOP_DATA_DIR`, flag first, then environment, then the
platform default. An override names yolop's directory itself rather than a
prefix, so nothing appends a second `yolop` folder to it, and a relative
override resolves against the startup working directory because turns may run
from a worktree elsewhere.

This is what lets several yolop identities run side by side, and lets a test
run touch nothing real, without moving `HOME`. Because the crash reporter and
the contributed-CLI registry both resolve paths before the command line is
parsed, the flags are read straight from argv as the process's first act rather
than from clap matches, so no global path escapes them. Directories belonging to
other applications are never redirected, and neither is the cross-agent
`~/.agents/skills` convention, which is shared with other agents on purpose and
has its own `YOLOP_GLOBAL_SKILLS_DIR` override for anyone who wants it moved
too.

### Named execution profiles

`--profile <name>` loads `profiles/<name>.toml` under the config root. Profile selection is explicit for
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

The profileable keys are `default_provider`, `models`, `default_models`, `base_urls`,
`approval_mode`, `approval_policy`, `sandbox_mode`, `worktrees`,
`capabilities`, `capabilities_mode`, `mcp`, `mcp_mode`, `instructions`,
`instructions_file`, and `skills_dir`. Credentials (`tokens`, `codex_auth`) and
personal settings (`theme`, `attribution`, `proactive_wake`, `acp_setup_page`)
are global-only and make a selected profile fail validation. Invalid known
values also fail startup; unknown keys produce a warning and are ignored for
forward compatibility.

### A profile is the unit of a purpose-built agent

v1 profiles overlaid *execution* settings only, which was enough to switch
provider or paranoia level but not to define an agent with a job. The keys above
close that gap, because everything that decides what an agent can do is now
selectable per run:

- **`capabilities`**, the same ordered `[[capabilities]]` list global settings
  carry, applied after the global entries. Since an installed extension's
  enablement *is* a `[[capabilities]]` entry, this is also how a profile turns
  extensions on and off. `capabilities_mode = "replace"` makes the profile's
  list the whole set, for an agent whose tool surface must not drift with the
  user's global preferences.
- **`mcp`**, `[mcp.servers.<name>]` entries merged by name over the global ones,
  or the whole set under `mcp_mode = "replace"`. Precedence is unchanged
  otherwise: workspace `.mcp.json` still overlays the result, and an ACP
  client's `session/new` servers still win over both.
- **`instructions`** (inline) and **`instructions_file`**, appended to the
  harness system prompt after `system.md` and the capability blocks. This is the
  standing job of the profile, not a durable user preference; the boundary with
  memory and `AGENTS.md` below still holds.
- **`skills_dir`**, an extra skills scope, defaulting to
  `profiles/<name>/skills/` when that directory exists. It ranks between the
  workspace and global scopes: a profile is chosen per run, so it outranks the
  user's global skills but never the repository's own. See
  [skills](skills.md).

Relative `instructions_file` and `skills_dir` paths resolve against the
`profiles/` directory, so a profile is a `.toml` file plus an optional sibling
directory of its assets. A named `instructions_file` that cannot be read fails
startup rather than silently dropping the job it describes.

Writes follow the same rule as the v1 keys: with a profile active, capability
enable/disable and capability config land in the profile, and disabling
something the global layer (or the harness default) turns on records an explicit
`enabled = false` mask rather than editing global settings. `instructions_file`
and `skills_dir` are pointers, so a write carries them through verbatim instead
of rewriting the path or inlining the file. `yolop mcp` and `/mcp` remain
scope-explicit (global settings or workspace `.mcp.json`); a profile's servers
are edited in the profile file.

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

### Attached command

`ConfigCapability` owns the attached `config` CLI. `yolop config get [key]`,
`set KEY VALUE`, and `clear KEY` manage settings, while
`yolop config model show|set MODEL|clear` manages the persistent model selection.
The plural `yolop config models ...` form delegates list reads and edits to
`ModelListCapability`; bare `models` is shorthand for `models list`. The same
capability owns attached `yolop model` and `yolop model use TARGET`: the first
reports the current session choice and the second changes that live session only.
Persistent defaults remain exclusively under `yolop config model show|set|clear`.
The capability exposes no agent tools.

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
`Arc<dyn ConfigService>`; write-coupled capabilities hold both, reads go
through the service handle, writes through the concrete `SettingsStore`, so the
read/write split is explicit at the type level. `AttributionCapability` reads
whether attribution is enabled through the service; `ApprovalCapability` reads
its soft-approval paranoia level through `ConfigService::approval_mode()` each
turn; `SetupController` reads provider/token/model state through its config
handle while persisting `/setup` changes through the store. `approval_mode`
remains a first-class schema key for internal configuration consumers.

The per-target read helpers (`current_value`, `scoped_current`) live in the
service module so internal configuration consumers share one implementation;
secret redaction and unsupported-provider filtering therefore apply uniformly
wherever config is read.

### Context delivery

The configuration capability does not add tools or an always-on system-prompt
block. The bundled `yolop-config` skill is the on-demand reference for the
attached model commands and adjacent configuration surfaces.

## Boundaries

Configuration is distinct from the neighbouring personalization surfaces:
durable preferences are **memory**, behavioral rules are **hooks**, and
guided provider setup is **`/setup`** in the TUI and **`yolop setup`** when
attached from another terminal. The attached family also provides `setup status`,
`setup login PROVIDER`, and `setup reauthenticate PROVIDER`. Live model selection
uses `yolop model use`; persistent selection and model-list edits remain under
the attached `config` command.
guided provider setup is **`/setup`**. Model selection and model-list edits go
through the attached `config` command; other settings retain their dedicated
setup and control surfaces.

Hook configuration is managed under the configuration command: `yolop config hooks list|get|set|remove`. Skill packages use the separate top-level `yolop skills ...` management command.

## Detached management commands

`yolop config hooks` manages global and workspace hook files through the same `HooksStore` used at startup. Skill package administration is a separate top-level `yolop skills` command. Neither management surface is registered as model tools.

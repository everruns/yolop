# Optional capabilities

Status: v1 implemented.

## Why

yolop is built on `everruns-runtime`, whose capability registry ships far more
than a terminal coding agent needs by default. The default surface is curated
deliberately tightly (see `src/runtime.rs`), but one size does not fit every
session: a user on a plane wants the network tools gone, while a long data
wrangling task benefits from scratch storage the default set omits. Hardcoding
the set forces a fork-or-nothing choice.

Optional capabilities make the curated set *adjustable* without giving up the
curation: a small catalog of vetted toggles, each with an explicit default,
flipped through the same schema-described configuration surface as every other
setting.

## What

### Catalog — single source of truth

`src/capabilities/optional.rs` is a compile-time catalog of
`OptionalCapabilitySpec`s: a user-facing `name`, `title`, `description`, and
`default_enabled`. The catalog feeds every surface — settings validation, the
`capabilities.<name>` config keys, `get_config` discovery output, and the
runtime gating — so there is no second list to keep in sync.

Catalog names are user-facing (`web_search`), not necessarily the upstream
capability id (`duckduckgo`); the id mapping (and any capability config, e.g.
`web_fetch`'s file-download flag) lives in `coding_harness_capabilities`.

Current catalog (defaults chosen so the out-of-the-box surface is unchanged
except by explicit opt-in/out):

| Name              | Default | Backing capability                              |
|-------------------|---------|--------------------------------------------------|
| `web_search`      | on      | `duckduckgo` (free web search, no API key)        |
| `web_fetch`       | on      | `web_fetch` (URL fetch + file download)           |
| `tool_search`     | on      | vendored `tool_search` (deferred tool loading)    |
| `current_time`    | off     | `current_time` (date already in env context)      |
| `session_storage` | off     | `session_storage` (`kv_store` / `secret_store`; in-memory, per-run) |

### Mechanism

- **Persistence** — a `[capabilities]` table in `settings.toml`, one bool per
  catalog name. Absent names fall back to the catalog default, so the file
  stays sparse and new catalog entries get sane defaults retroactively.
- **Configuration** — `capabilities.<name>` is a first-class schema key
  (`KeyScope::Capability`, validated against the catalog the same way provider
  keys validate against the provider list). `set_config` flips a toggle;
  `value=clear` removes it so the default applies again; `get_config
  key=capabilities` renders the whole catalog with effective values, defaults,
  and descriptions — it is the discovery surface.
- **Activation** — `coding_harness_capabilities` reads the toggles once per
  runtime build. Implementations stay registered in the `CapabilityRegistry`
  unconditionally (a registered id is inert until the harness capability list
  enables it), so the settings toggle is the single switch. Changes apply on
  the next run.

## Evaluated and deliberately excluded

Reviewed against the everruns-core 0.10 catalog; revisit when upstream or the
in-process runtime changes:

- **`subagents`** — `spawn_subagent` creates a child session, but nothing in
  the in-process runtime drives a child session's agent loop (the hosted
  worker does that). Add once upstream supports in-process child sessions.
- **`session_sql_database`** — its tools need `ToolContext::sqldb_store`,
  which `InProcessRuntime` does not wire (only the hosted adapter does).
- **`lua` / `lua_code_mode`** — experimental upstream, High-risk/admin-gated;
  reconsider when it stabilizes (its goal is to supersede `virtual_bash`,
  which yolop does not use either — `yolop_bash` runs on the host).
- **`persistent_memory`** — overlaps yolop's `your` memory (specs/your.md),
  and the in-process backend is non-persistent anyway.
- **`btw`** — has no `execute_command`; the hosted server implements `/btw`
  with a bespoke executor, so dispatching it here would error.
- **`auto_tool_search` / `openai_tool_search`** — superseded by the vendored
  `tool_search` (specs/tool-search.md), which works on every provider.
- **`background_execution`** — auto-activates only when a tool opts in via
  `ToolHints::supports_background`; none of yolop's tools do yet.
- **`self_budget`** — prompt-only but depends on `get_session_info` from the
  `session` capability, which yolop does not enable.
- **Hosted-platform capabilities** (`budgeting`, `knowledge_base`,
  `a2a_delegation`, `agent_handoff`, `workspace_volumes`, `session_sandbox`,
  `session_schedule`) — require server-side stores/services that have no
  in-process equivalent.

## Boundaries

The catalog is for *agent capabilities* (tools/prompt contributions the
session exposes). Behavior tuning stays where it is: soft approval is
`approval_mode`, attribution is `attribution`, hooks are the hooks config, MCP
servers are `.mcp.json`. Mandatory plumbing (filesystem, instructions,
compaction, loop detection, …) is not toggleable — a toggle that breaks the
agent is not an option worth carrying.

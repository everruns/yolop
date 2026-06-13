# `your` — personalization

Status: implemented (framing + hooks). Durable memory now lives in the `memory`
capability ([`memory.md`](./memory.md)); roadmap sections below are not yet built.

## Why

Yolop today only personalizes one thing: the LLM provider/token, via the
`/provider` and `/token` slash commands. Everything else — durable user
preferences, global skills, future hooks — has nowhere to live. Users also
have no natural-language way to configure yolop itself; they have to remember
slash-command syntax.

`your` is the personalization layer. The name is how a user addresses yolop
itself: *"what is **your** config?"*, *"update **your** memory"*, *"set yolop
blue"*, *"remember that I prefer terse answers"*. These are **global**
requests about yolop the tool, and must be distinguished from changes to the
current project (which belong in the repo's `AGENTS.md`, source, and tests).

The long-term goal: `your` can configure **any** aspect of yolop, in natural
language, backed by durable state in a central location.

## What — scope of the layer

Everything `your` owns lives under the platform config dir, alongside the
existing `settings.toml`:

| OS      | Central dir                                 |
|---------|---------------------------------------------|
| Linux   | `~/.config/yolop/`                          |
| macOS   | `~/Library/Application Support/yolop/`       |
| Windows | `%APPDATA%\yolop\`                           |

```
<config_dir>/yolop/
  settings.toml      # provider + tokens (pre-existing)
  MEMORY.md          # durable cross-session user memory (owned by the `memory` capability)
  skills/            # global, always-available skills
```

The capability must always be able to **describe itself** — what it is, where
its state lives, and what it can currently do — to the model through the system
prompt and tools, and to the user through natural-language questions such as
"what is your config?"

## Memory

Durable, cross-session user memory has moved into its own `memory` capability —
structured `MEMORY.md`, progressive disclosure, and the `remember` / `recall` /
`forget` tools. See [`memory.md`](./memory.md). `your` keeps only the
personalization *framing*: it decides when a request is global personalization
and routes "remember that I prefer X" to the `remember` tool, rather than owning
the store itself.

## Self-Configuration Pattern

Natural language is the user interface, but durable configuration changes
should go through purpose-built tools. The `your` prompt decides whether a user
is asking to configure yolop itself; embedded `your` skills can provide
examples and recipes; tools perform validated writes to stable config files.

For example, "yolop setup a hook to prevent calls to git" is a
self-configuration request. `your` should translate it into a hook spec,
validate it, and write it through hook config tools rather than storing a
memory note that says "avoid git". See [`hooks.md`](./hooks.md) for the hook
tool design and scope rules.

## Roadmap

The roadmap rides on everruns' existing extension points rather than inventing
yolop-specific formats.

- **User-defined capabilities** — the central piece. everruns already has a
  first-class *declarative capability*: a serializable
  `DeclarativeCapabilityDefinition` (capability id `declarative:<name>`) that
  contributes a `system_prompt`, mounted `files`, `skills` (name +
  description + instructions + bundled files), and `mcp_servers` — entirely
  from data, no compiled code. `your` stores user definitions under the
  central config dir and registers them into every session, so a user can add
  global capabilities without rebuilding yolop. Compiled (built-in)
  capabilities remain the path for behavior that needs real Rust.
- **Declarative capability skills** — global skills are already discovered
  directly from `<config_dir>/yolop/skills`. A future declarative capability
  layer can also contribute skills into that same global personalization model.
  This is where memory that has outgrown a few bullets gets promoted to.
- **Hooks** — `your` should configure global and workspace hooks through
  structured hook tools, using the scope and merge contract in
  [`hooks.md`](./hooks.md).
- **General config** — map natural-language requests ("set yolop blue") onto
  real settings as those settings come to exist. Until a knob exists, the
  preference is recorded in memory and honored on a best-effort basis.

## Non-goals

- Not a secret store — tokens stay in `settings.toml` under `/token`.
- Not project memory — repo-scoped guidance stays in `AGENTS.md`.
- Not the memory store itself — durable memory is the `memory` capability's
  job ([`memory.md`](./memory.md)).

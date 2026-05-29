# `your` — personalization

Status: v1 implemented (memory core). Roadmap sections below are not yet built.

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
  MEMORY.md          # v1: durable cross-session user memory
  skills/            # roadmap: global, always-available skills
```

The capability must always be able to **describe itself** — what it is, where
its state lives, and what it can currently do — both to the user (`/your`) and
to the model (a system-prompt block).

## v1 — central memory

`MEMORY.md` is a single markdown file of durable, global user preferences and
facts ("I prefer terse answers", "always run `cargo fmt` before committing",
"my name is Mike"). It is **injected into every turn** so the model honors
preferences without being reminded.

Distinct from `AGENTS.md`: `AGENTS.md` is per-project guidance committed to a
repo; `MEMORY.md` is per-user, global, and never committed.

### Managed size

Unbounded memory would bloat every prompt. So:

- A **byte budget** caps how much is injected. Beyond it, injection is
  truncated at a char boundary with a visible notice — the model is told the
  memory was clipped and to use `read_memory` for the full text.
- A **soft limit** (below the hard budget) triggers a standing suggestion, in
  both the injected block and tool results, to extract stable, topic-specific
  guidance into a **skill** rather than letting memory grow without bound.
  Skills are the pressure-release valve for memory that has outgrown a few
  bullet points.

### Natural-language configuration

The model configures memory through tools, driven by ordinary chat — no slash
syntax required:

- `remember` — append a durable preference/fact.
- `read_memory` — read the full file (needed when injection was truncated, or
  to edit precisely).
- `write_memory` — replace the whole file, for reorganizing or removing.

`/your` prints the capability's self-description and current memory status
(path, size, budget state). It also works in `--print` mode where the model
isn't driving.

## Roadmap (not yet implemented)

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
- **Global skills** — fall out of the above: a declarative capability's
  `skills` are mounted at everruns' `SKILLS_DISCOVERY_PATH`
  (`/.agents/skills`) so the built-in `skills` capability lists/activates
  them in every session. This is also where memory that has outgrown a few
  bullets gets promoted to.
- **Hooks** — once yolop grows a hook system, `your` configures global hooks
  from the same central dir (and, where everruns exposes hook contributions
  on capabilities, via declarative definitions).
- **General config** — map natural-language requests ("set yolop blue") onto
  real settings as those settings come to exist. Until a knob exists, the
  preference is recorded in memory and honored on a best-effort basis.

## Non-goals

- Not a secret store — tokens stay in `settings.toml` under `/token`.
- Not project memory — repo-scoped guidance stays in `AGENTS.md`.
- No retention/rotation policy; `MEMORY.md` is a plain file the user owns.

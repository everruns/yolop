# `memory` — global, durable user memory

Status: implemented.

## Why

Yolop needs to remember durable, cross-session facts about the user — "prefer
terse answers", "my name is Mike", "always run `cargo fmt` before committing" —
so the model honors them without being reminded each session.

The first cut lived inside the `your` personalization capability as a single
flat `MEMORY.md` bullet list, injected **in full every turn** under a byte
budget. That has two problems: the whole file competes for prompt space no
matter how little of it is relevant, and a flat bullet list has no structure to
search, timestamp, or address individual notes.

`memory` is its own capability. It owns structured, durable memory and the
tools to manage it; `your` keeps the personalization *framing*, while hook
self-configuration lives in the `hooks` capability (see [`hooks.md`](./hooks.md)).

## What

`MEMORY.md` lives in the central config dir beside `settings.toml`
(`~/.config/yolop/MEMORY.md` on Linux) and is never committed to a repo —
distinct from `AGENTS.md`, which is per-project guidance.

It is **structured**: each memory is a `## ` section with

- a **title** — a short description of the memory; doubles as the search key
  and the disclosed label,
- a generated **id** (`m-1a2b3c`) plus **created**/**updated** timestamps,
  carried in an HTML comment, and
- a **body** — the memory text itself.

```markdown
## Prefer terse answers
<!-- id: m-1a2b3c · created: 2026-06-13T03:54:00Z · updated: 2026-06-13T03:54:00Z -->

The user prefers terse, factual answers without preamble.
```

The file is the durable store; an in-memory `Vec<Memory>`, kept newest-first by
`updated`, is the source of truth at runtime. Writes are atomic (temp file +
`fsync` + `rename`, `0o600` on Unix) so readers and crashes never see a
half-written file. A legacy flat-bullet file is imported wholesale as a single
"Imported notes" memory on first open, so an upgrade never loses notes.

## Progressive disclosure

Only memory **titles** are injected into the system prompt each turn (the most
recent `disclosed_titles`, newest first), with the id and date. Bodies are
**not** injected. The block tells the model how many memories exist in total and
to use `recall` to read full text or find the rest. This keeps prompt cost flat
no matter how much the user has stored — the opposite of injecting the whole
file.

## Tools

- `remember(title, memory, id?)` — store a memory. The timestamp is set
  automatically. Passing an existing `id` — or reusing an existing title
  (case-insensitive) — updates in place instead of duplicating.
- `recall(query?, id?, limit?)` — read memory. An `id` fetches one memory's
  full text; a `query` searches; with neither, the most recent memories are
  returned. Results are **capped** at `limit` (default `recall_limit`), and when
  more match, the response says so — the model is told to narrow the query
  rather than try to read everything. There is deliberately no "dump the whole
  file" tool.
- `forget(id)` — delete one memory by id (preferred) or exact title.

### Search ranking

Query tokens are matched case-insensitively. A token in a memory's **title**
scores 3; in its **body**, 1; summed across tokens. Ties break toward the most
recently updated memory, so search "prefers latest". An empty query falls back
to most-recent order.

## Configuration

Tuning flows through the **generic capability-config system**, not a bespoke
settings key: the capability publishes a `config_schema()` and reads its values
from the per-agent `AgentCapabilityConfig.config` (consumed via
`tools_with_config` / `system_prompt_contribution_with_config`). This is the
same mechanism `agent_instructions` and `user_hooks` use, so a generic settings
editor can drive it without hard-coding `memory`.

| Key                | Default | Meaning                                                  |
|--------------------|---------|----------------------------------------------------------|
| `disclosed_titles` | 15      | Memory titles injected into the prompt each turn.        |
| `recall_limit`     | 5       | Default number of memories `recall` returns.             |
| `soft_cap`         | 200     | Warn (never delete) once more than this many exist.      |

Missing or null config falls back to defaults; `validate_config` rejects wrong
types on the write path, while the per-turn read paths fall back to defaults and
log on bad input rather than dropping the capability.

`soft_cap` is a **soft** limit: once memory grows past it, `remember`/`recall`
nudge the model to `forget` stale memories or promote stable guidance to a
skill. Yolop never silently drops a user's memory. Setting it to `0` disables
the warning.

## Non-goals

- Not a secret store — tokens stay in `settings.toml`.
- Not project memory — repo-scoped guidance stays in `AGENTS.md`.
- No automatic retention/rotation; `MEMORY.md` is a plain file the user owns.

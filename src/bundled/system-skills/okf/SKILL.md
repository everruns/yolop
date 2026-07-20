---
name: okf
description: Read, author, and validate Open Knowledge Format (OKF) bundles — knowledge represented as a directory of markdown files with YAML frontmatter. Use when a repo contains an OKF bundle (concept docs with a `type` field, an `index.md`/`log.md`), when the user mentions OKF or the Open Knowledge Format, or when asked to author, validate, convert to, or keep an OKF knowledge base current.
user-invocable: true
---

# Open Knowledge Format (OKF)

OKF is an open spec (Google Cloud, v0.1) for representing knowledge as a plain
**directory of markdown files with YAML frontmatter** — no database, SDK, or
runtime. If you can `cat` a file, you can read OKF. This skill carries the whole
mental model inline so it works offline; consult the spec only for edge cases.

Spec: <https://okf.md/spec/> · Overview: <https://okf.md/>

## The model (all of it)

- **Bundle** — a directory tree of markdown; the unit of distribution. There is
  **no required root marker file** and no mandated directory name, so a bundle is
  simply a directory someone declares to be OKF (often `.okf/`, `okf/`, or
  `knowledge/`, but that is convention, not spec).
- **Concept** — one `.md` file = one unit of knowledge: a table, an API, a
  metric, a process, an idea.
- **Concept-id** — the file path within the bundle minus `.md`
  (`tables/users.md` → `tables/users`).
- **Links** — plain markdown links between concepts turn the tree into a graph.
  Prefer bundle-absolute paths starting with `/` (`[users](/tables/users)`).
- **Reserved files** (both optional):
  - `index.md` — a directory listing for **progressive disclosure**: read it
    first to see what a bundle holds without opening every file.
  - `log.md` — a chronological changelog, newest-relevant, dated ISO 8601.

## Frontmatter

Every concept file has YAML frontmatter. Only one field is **required**:

- `type` (required) — a short producer-chosen string, e.g. `BigQuery Table`,
  `Metric`, `Playbook`. Values are not centrally registered.

Recommended optional fields: `title`, `description` (one sentence), `resource`
(a URI for the underlying asset), `tags` (list), `timestamp` (ISO 8601 of last
significant change). Producers may add more keys; **consumers must preserve
unknown keys** and must not choke on them.

```markdown
---
type: Module
title: tuika
description: Standalone terminal-UI toolkit powering the --fullscreen renderer.
resource: file:///crates/tuika
tags: [ui, ratatui, terminal]
timestamp: 2026-07-19T10:30:00Z
---

# tuika

Layout, overlays, focus, and components over ratatui.

## Relationships
- Consumed by the [yolop host](/modules/yolop-yep)
```

## Conformance (the only hard rules)

A bundle conforms to OKF v0.1 if:

1. Every non-reserved `.md` file has parseable YAML frontmatter.
2. Every such frontmatter has a non-empty `type` field.
3. Reserved files (`index.md`, `log.md`) follow their structure.

Everything else is **soft guidance**. Do not reject a bundle for missing
optional fields, unknown `type` values, broken links, or an absent `index.md` —
the format is deliberately permissive. Validate with `scripts/validate_okf.py`
under `${SKILL_DIR}` (no dependencies): `python3 ${SKILL_DIR}/scripts/validate_okf.py <bundle-dir>`.

## Consuming a bundle

When a repo contains an OKF bundle, treat it as curated, high-signal context:

1. Read `index.md` at the bundle root first — it maps the territory cheaply.
2. Open only the concept docs relevant to the task; follow links to related
   concepts instead of grepping blindly.
3. Check `log.md` when recency matters.

## Authoring a concept

1. Pick a stable concept-id (its path); group related concepts in subdirectories.
2. Write frontmatter — always `type`; add `title`/`description`/`timestamp` for
   anything others will read.
3. Write the body as normal markdown; link related concepts with `/`-absolute
   paths so links survive moves.
4. Keep `index.md` and `log.md` current when you add or change concepts.

## Producing / maintaining a bundle in this repo

If this repo asks yolop to keep a knowledge bundle current (e.g. an `.okf/`
directory referenced from `AGENTS.md`), after a change that alters durable facts
about the codebase:

- Add or update the affected concept file(s), bumping their `timestamp`.
- Update `index.md` if the set of concepts changed.
- Append a dated entry to `log.md`.

Write down only durable, reusable knowledge — architecture, contracts,
invariants, processes — not transient task notes. A bundle that rots into stale
docs is worse than none; keep it accurate or leave it out.

## Converting into OKF

From a wiki, Notion export, Obsidian vault, or CSV: one source page/row → one
concept file; choose a `type`; carry titles/descriptions into frontmatter;
rewrite internal links as `/`-absolute concept links; add an `index.md`.

---
name: okf
description: Read, author, maintain, and validate Open Knowledge Format (OKF v0.2) bundles, knowledge as a directory of markdown files with YAML frontmatter, carrying provenance, trust, lifecycle, and attested computations. Use when a repo holds an OKF bundle (concept docs with a `type` field, an `index.md`/`log.md`), when the user mentions OKF or the Open Knowledge Format, or when asked to author, validate, convert to, or keep an OKF knowledge base current.
user-invocable: true
---

# Open Knowledge Format (OKF) v0.2

OKF represents knowledge as a plain **directory of markdown files with YAML
frontmatter**: no database, SDK, or runtime. If you can `cat` a file, you can
read OKF; if you can `git clone` a repo, you can ship it.

The one authoritative spec is `SPEC.md` in the Google Cloud knowledge-catalog
repository:
<https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md>.
Treat any other site or summary as hearsay. This skill carries the working model
inline so it functions offline; read the spec itself for edge cases.

The point of v0.2 is that a corpus is now mostly **written and maintained by
agents**, so a reader needs frontmatter answers to: what was this made from
(provenance), how much should I trust it (trust), is it still true (freshness),
and was this number computed the sanctioned way (attestation).

## The model

- **Bundle**: a directory tree of markdown; the unit of distribution. There is
  **no root marker file and no mandated directory name**: a bundle is whatever
  directory someone declares to be one (`knowledge/`, `okf/`, `.okf/` are
  conventions, not spec). Never guess, take the location from the user or from
  `AGENTS.md`.
- **Concept**: one `.md` file = one unit of knowledge: a table, an API, a
  metric, a process, an idea.
- **Concept ID**: the file path within the bundle minus `.md`
  (`tables/users.md` → `tables/users`).
- **Links**: ordinary markdown links between concepts make the tree a graph.
  Prefer bundle-absolute paths starting with `/` (`[users](/tables/users.md)`);
  relative paths are also valid. A link asserts an untyped relationship; the
  prose around it says what kind.
- **Reserved files** (both optional, at any directory level, never concepts):
  - `index.md`, a directory listing for **progressive disclosure**: read it
    first to see what a bundle holds without opening every file. No frontmatter,
    except that the bundle-root `index.md` may carry `okf_version: "0.2"`.
  - `log.md`, dated change history, newest first, `## YYYY-MM-DD` headings.

## Frontmatter

Only `type` is required. A concept carrying nothing but `type` is fully
conformant.

| Field | Status | Notes |
|---|---|---|
| `type` | **required** | Producer-chosen string: `Metric`, `BigQuery Table`, `Playbook`, `Attested Computation`. Not centrally registered. |
| `title` | recommended | Display name; consumers may fall back to the filename. |
| `description` | recommended | One sentence; feeds `index.md` entries and search snippets. |
| `resource` | recommended | URI of the underlying asset. Absent for abstract concepts. |
| `tags` | recommended | List of short strings. |

Producers may add any other key. As a consumer, **preserve unknown keys** when
round-tripping and never reject a document for carrying them.

Bodies are plain markdown; prefer headings, lists, tables, and fenced code over
freeform prose. `# Schema`, `# Examples`, and `# Computation` are conventional
headings, use them when they apply.

```markdown
---
type: Module
title: tuika
description: Standalone terminal-UI toolkit powering the --fullscreen renderer.
resource: file:///crates/tuika
tags: [ui, terminal]
status: stable
generated: { by: human:mchaliy, at: 2026-07-19T10:30:00Z }
---

# tuika

Layout, overlays, focus, and components for terminal UIs.

# Relationships

Consumed by the [yolop host](/modules/yolop-yep.md).
```

## Provenance, trust, lifecycle

All optional, but their **absence carries meaning**, so populate them for
anything an agent generated. Never reject a concept for lacking them.

**`sources`**: the materials a concept derives from:

```yaml
sources:
  - id: ga4-schema                     # stable key for per-claim attribution
    resource: https://developers.google.com/analytics/bigquery/export-schema
    title: GA4 BigQuery Export schema  # required within an entry: resource
    author: team:ga4-docs              # credibility signal: authority
    usage_count: 5000                  # credibility signal: adoption/liveness
    last_modified: 2026-05-30          # credibility signal: recency of the source
usage_window: { from: 2026-06-01, to: 2026-06-30 }   # frames every usage_count
```

`resource` may name a followable artifact (URL, bundle path, `references/` file)
or a scope descriptor it cannot follow (`all queries in project X`). OKF stores
objective **signals**, never a credibility score, infer, do not persist a
verdict. Read `usage_count` as liveness and trend, not a precise ranking.
Lineage is expressed by linking: when a `resource` points at another concept,
recurse into its own `sources`.

Attribute a specific claim with a markdown footnote whose label **is** a
`sources[].id`, keyed, so reordering the list cannot misattribute:

```markdown
The `events_` table is sharded daily as `events_YYYYMMDD`.[^ga4-schema]

[^ga4-schema]: GA4 BigQuery Export schema
```

**`generated` and `verified`**: who wrote it versus who confirmed it, kept
deliberately separate:

```yaml
generated: { by: reference_agent/gemini-2.5-pro, at: 2026-06-20T22:53:05Z }
verified:
  - { by: human:ahormati, at: 2026-06-25T09:00:00Z }
  - { by: process:finance-nightly, at: 2026-06-26T02:00:00Z }
```

`generated.by` is required within `generated`; `generated.at` marks the last
meaningful content change. A bare `verified: { by, at }` mapping **must** be
read as a one-element list. Actors follow one convention: `<producer>/<version>`
for agents, `human:<id>` for people, `process:<id>` for automated processes. The
`human:` prefix is essential, trust tiers key off it:

- no `verified` ⇒ **unverified**
- `verified` by non-`human:` actors only ⇒ **machine-confirmed**
- any `human:<id>` verifier ⇒ **human-reviewed**

**Lifecycle**: `status: draft | stable | deprecated` (absent ⇒ `stable`) and
`stale_after: YYYY-MM-DD`, an absolute date; a concept is stale when
`today >= stale_after`.

## Attested computations

A sanctioned computation is its own concept, `type: Attested Computation`, that
value-consuming concepts (a `Metric`, a table) link to. Provenance answers
"where did this claim come from"; attestation answers "was this number produced
the way we said it must be".

```yaml
runtime: bigquery                      # REQUIRED for this type; defines what parameters mean
parameters:
  - { name: year, type: integer, required: true }
computation: references/computations/revenue.sql   # or omit and inline under `# Computation`
executor:
  resource: references/skills/run-on-bq.md         # run instructions/code
  receipt: [job_id, executed_sql, result]          # evidence a run must return
attester:
  resource: references/attesters/sql-equality.py   # deterministic, no-LLM check
```

Rules that matter when you are the agent in the loop:

- You may supply **values** for declared `parameters` only. You **must not**
  author or edit the computation, rewriting it is exactly what attestation
  detects.
- Discover → load contract → parameterize → execute (executor returns a receipt)
  → attest (deterministic code compares the expanded artifact against the
  sanctioned one) → gate. Refuse to present a failing attestation; warn when
  `today >= stale_after`. Receipts and verdicts are runtime artifacts, never
  write them into the bundle.
- `verified` and attestation are both needed: `verified` says the *definition*
  still matches policy, attestation says a *single run* was honest.

Each computation carries its own trust state, so revenue can be fresh while
profit is stale. One figure, one concept.

## Conformance

A bundle conforms if, and only if:

1. Every non-reserved `.md` file has a parseable YAML frontmatter block.
2. Every such frontmatter has a non-empty `type`.
3. Present `index.md`/`log.md` files follow their structures.

Everything else is soft guidance. Do **not** reject a bundle for missing
optional fields, unknown `type` values, unknown frontmatter keys, broken links
(they may be not-yet-written knowledge), or an absent `index.md`. Unrecognized
`okf_version` ⇒ best-effort consumption, not refusal.

Check with the bundled zero-dependency validator:

```bash
python3 ${SKILL_DIR}/scripts/validate_okf.py <bundle-dir>                  # the 3 rules
python3 ${SKILL_DIR}/scripts/validate_okf.py <bundle-dir> --strict --check-links
```

`--strict` adds producer-side lint (`title`, `description`, `generated`,
`runtime` on attested computations, `status`/date shapes, legacy `timestamp`);
`--check-links` resolves intra-bundle link targets. Both go beyond conformance,
their findings are reasons to improve a bundle you own, never to reject one you
are merely reading.

## Consuming a bundle

Treat it as curated, high-signal context that beats grepping:

1. Read the bundle-root `index.md` first, it maps the territory cheaply.
2. Open only the concepts the task touches, then follow their links.
3. Weigh before relying: `status: deprecated` or `today >= stale_after` means
   verify against the real system before acting; an unverified, agent-generated
   concept is a lead, a human-reviewed one is a fact.
4. Read `log.md` when recency or "what changed" matters.

## Authoring and maintaining

1. Pick a stable concept ID (its path); group related concepts into
   subdirectories with their own `index.md`.
2. Write frontmatter: always `type`; add `title`/`description` for anything
   others read; set `generated: { by, at }` with the correct actor, use
   `human:<id>` only for genuinely human-authored content.
3. Write a structured body; link related concepts with `/`-absolute paths so
   links survive moves. Cite claims with footnotes keyed to `sources[].id`.
4. Keep `index.md` (when the concept set changes) and `log.md` (a dated entry)
   current in the same change.

When a repo asks yolop to keep its bundle current, update the affected concepts
after any change to durable facts, refresh `generated.at`, and record it in
`log.md`. Write down only durable, reusable knowledge, architecture, contracts,
invariants, processes, never transient task notes. A bundle that rots into
stale docs is worse than none.

## Converting into OKF

From a wiki, Notion export, Obsidian vault, or CSV: one source page/row → one
concept file; pick a descriptive `type`; carry titles and summaries into
`title`/`description`; rewrite internal links as `/`-absolute concept links;
record where each concept came from in `sources` and stamp `generated`; add an
`index.md`. Migrating a v0.1 bundle: `timestamp` → `generated: { by, at }`, and
a body `# Citations` list → frontmatter `sources`. Everything else carries over
unchanged.

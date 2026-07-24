---
type: Product Specification
title: `okf` — native Open Knowledge Format support
description: Defines the `okf` — native open knowledge format support contract for Yolop.
---

# `okf` — native Open Knowledge Format support

Status: implemented as a bundled skill in `src/bundled/system-skills/okf/`.
**No runtime capability** — see the rationale below.

## Why

[OKF](https://okf.md/spec/) (Open Knowledge Format, Google Cloud, v0.1)
represents a body of knowledge as a plain directory of markdown files with YAML
frontmatter — no database, SDK, or runtime. It is a portable way to hand an
agent curated context: architecture, data models, metrics, processes,
conventions. Yolop should be able to read such a bundle as high-signal context,
and author/validate one on request, without the user re-explaining the format.

## What

A single system skill, `okf`, compiled into the binary so it works regardless of
network policy. Its `SKILL.md` carries the whole v0.1 mental model inline —
bundle, concept, concept-id, links, the reserved `index.md`/`log.md`, the
required `type` frontmatter field plus recommended fields, and the three
conformance rules — and links the spec for depth. It ships a zero-dependency
validator (`scripts/validate_okf.py`, `--strict` and `--check-links` modes).

The skill covers reading, authoring, converting to (Notion/Obsidian/CSV), and
validating OKF. Because it teaches authoring, **producing/maintaining** a bundle
needs no separate feature: a repository can instruct the agent (e.g. from
`AGENTS.md`) to keep its bundle current, and the agent does so through the skill.

## Why no detection capability

An earlier iteration shipped a capability that scanned for a bundle at session
start and injected a system-prompt note. It was removed, deliberately:

- **OKF defines no canonical bundle marker.** The spec mandates only a per-file
  `type` field — no required root manifest and no fixed directory name. Any
  automatic detection therefore has to *guess* a location.
- The guessing meant inventing conventions (`.okf/`, `okf/`) that are **not part
  of the standard**, and leaning on a `YOLOP_OKF_BUNDLE_DIR` escape hatch. Both
  overclaimed a standard that does not exist and put unearned weight on an env
  var.

Yolop already re-reads `AGENTS.md` every turn and activates skills by relevance.
That is the honest, standard-agnostic path: a repository that keeps a bundle
simply points at it in `AGENTS.md` ("the knowledge bundle is in `./knowledge`"),
and the `okf` skill supplies the how-to when OKF work comes up. No invented
directory convention, no always-on probe.

## Non-goals

- No bundle-loading runtime, index, or database — OKF is "just markdown," and the
  file tools already read it.
- No new authoring tools in the binary; authoring lives in the skill.
- No automatic bundle detection or fixed directory convention (see above).

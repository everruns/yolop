---
type: Product Specification
title: `okf` — native Open Knowledge Format support
description: Defines the `okf` — native open knowledge format support contract for Yolop.
---

# `okf` — native Open Knowledge Format support

Status: implemented as a bundled skill in `src/bundled/system-skills/okf/`.
**No runtime capability** — see the rationale below.

## Why

[OKF](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)
(Open Knowledge Format, Google Cloud) represents a body of knowledge as a plain
directory of markdown files with YAML frontmatter — no database, SDK, or
runtime. It is a portable way to hand an agent curated context: architecture,
data models, metrics, processes, conventions. Yolop should be able to read such
a bundle as high-signal context, and author, maintain, or validate one on
request, without the user re-explaining the format.

## Authority

`SPEC.md` in the `GoogleCloudPlatform/knowledge-catalog` repository is the only
authoritative source, and it is what the skill tracks. Third-party sites that
restate the format — including the `okf.md` domain the skill previously cited —
are not normative and must not be linked as if they were; they lagged the spec
and described a superseded v0.1 model.

## What

A single system skill, `okf`, compiled into the binary so it works regardless of
network policy. Its `SKILL.md` carries the whole v0.2 mental model inline:

- Structure — bundle, concept, concept ID, links, the reserved `index.md` and
  `log.md`, and the fact that OKF defines no root marker or directory name.
- Frontmatter — the required `type`, the recommended
  `title`/`description`/`resource`/`tags`, and the rule that unknown keys are
  preserved rather than rejected.
- Provenance, trust, lifecycle — `sources` with per-source credibility signals
  and footnote attribution keyed to `sources[].id`, `generated`/`verified` with
  the actor convention and the trust tiers derived from it, `status`, and
  `stale_after`.
- Attested computations — the `Attested Computation` type, its `runtime`,
  `parameters`, `computation`, `executor`, and `attester` fields, and the
  agent-facing rule that only parameter values may be supplied, never edits to
  the computation.
- Conformance — the three hard rules, and the permissiveness that makes
  everything else soft guidance.

The version-specific families exist because an agent-maintained corpus needs
frontmatter answers to provenance, trust, freshness, and attestation. The skill
teaches them as *consumption* signals too: a deprecated or past-`stale_after`
concept is verified against the real system before it is acted on.

Because the skill teaches authoring, **producing/maintaining** a bundle needs no
separate feature: a repository can instruct the agent (e.g. from `AGENTS.md`) to
keep its bundle current, and the agent does so through the skill.

## Validator

The skill ships a zero-dependency validator, `scripts/validate_okf.py`. Its
default run enforces only the three conformance rules; producer-side lint —
`title`, `description`, `generated`, the `runtime` an attested computation
requires, the shapes of `status`/`stale_after`/`okf_version`, and the v0.1 fields
v0.2 superseded — sits behind `--strict`, and intra-bundle link resolution behind
`--check-links`. Splitting them this way keeps the tool from teaching a
permissive format as a strict one.

The repository's own copy at `scripts/validate_okf.py` and the skill's copy are
byte-identical, pinned by `scripts/test_validate_okf.py`, because a bundled
script that drifts from the one CI runs is worse than either alone.

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
- No execution or attestation runtime. The skill teaches the attested-computation
  contract and the gating rules; the executor and attester behind a `resource`
  are the consuming system's, not Yolop's.

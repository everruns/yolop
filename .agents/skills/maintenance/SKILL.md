---
name: maintenance
description: Goal-oriented repository maintenance and release-readiness work for yolop. Use when the user asks for maintenance, release prep, repo health review, dependency refreshes, knowledge/docs alignment, test gap review, or general cleanup without prescribing an exact sequence.
metadata:
  internal: true
user-invocable: true
---

# Maintenance

Goal: leave the repo materially healthier and closer to release-ready, with
evidence.

[`knowledge/specs/maintenance.md`](../../../knowledge/specs/maintenance.md) owns the success bar and the
rationale behind each surface. This skill owns how to work a pass.
[`surfaces.md`](surfaces.md) holds the per-surface commands and heuristics —
open it for the surfaces your scope actually covers.

## Scope

Use the scope the user gave; otherwise state the one you inferred before
starting. Typical scopes: release readiness, CI health on `main`, `everruns-*`
dependency refresh, knowledge or docs drift, feature-completeness drift across
CLI / TUI / knowledge / README / tests, test gaps, code simplification, security
hygiene, performance of recently changed code, binary size, AGENTS / skills /
command hygiene.

## Working a pass

A red CI on `main` outranks every other scope — fix it first, or open an issue
and report the pass **blocked**. Otherwise go highest-signal first: recent
diffs, failing checks, stale knowledge, outdated `everruns-*` versions.

Prefer fixing over reporting. Fix what is small and local; for anything larger,
write a crisp finding and defer it to a GitHub issue naming the problem and its
user-visible impact — then put the issue number in the report. Skipping a
surface is fine; skipping it silently is not.

Keep each change PR-sized and independently reviewable. Do not fold a
simplification sweep into an unrelated fix. When a bug surfaces, prefer the
failing test before the fix.

Validation matches the surfaces you touched — the
[checks in `AGENTS.md`](../../../AGENTS.md), plus a live-provider smoke through
Doppler when the pass touched runtime behavior. Do not declare release-ready
for surfaces you did not actually check.

## Report

- scope covered, and what was intentionally skipped with a reason
- what was fixed, and what was found
- evidence gathered
- deferred findings with their GitHub issue numbers
- **blocked** if `main` CI is red and out of reach

If the user asks to ship the result, hand off to [`/ship`](../ship/SKILL.md).

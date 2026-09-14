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
[`surfaces.md`](surfaces.md) holds the per-surface commands and heuristics,
open it for the surfaces your scope actually covers.

## Scope

Use the scope the user gave; otherwise state the one you inferred before
starting. Typical scopes: release readiness, CI health on `main`,
`everruns-*` and `tuika` refresh including deep major upgrades, YEP wire
compat, local inference matrix, binary size, knowledge or docs drift,
knowledge versus code conflicts, feature-completeness drift across CLI, TUI,
inline, print, ACP, skills, README, and tests, eval gaps, test review across
coverage, obviousness, duplication, cost, and nonsense, architecture,
soundness, and simplification, security reviews, README soundness,
performance of recently changed code, AGENTS, skills, and command hygiene.

## Working a pass

A red CI on `main` outranks every other scope. Fix it first, or open an issue
and report the pass **blocked**. Otherwise go highest-signal first: recent
diffs, failing checks, stale knowledge, outdated `everruns-*` or `tuika`
versions.

Prefer fixing over reporting, and keep findings in the pass. Fix what is
small and local; for anything larger, write a crisp finding and defer it to a
GitHub issue naming the problem and its user-visible impact, then put the
issue number in the report. A large diff, a long build, a missing audit, or
an unfamiliar failure is work to finish, not a reason to defer. Never weaken
a test, a security limit, or a supply chain criterion to ship an upgrade. If
the newest upstream version cannot preserve a required contract, establish
the incompatibility, retain the newest safe version with a tested and
documented pin, and complete the rest of the pass.

Keep each change PR-sized and independently reviewable. Do not fold a
simplification sweep into an unrelated fix. When a bug surfaces, prefer the
failing test before the fix. Review touched tests on the merits: coverage of
the behavior, whether the test can fail for a real reason, duplication, and
cost in speed and flakiness. Skipping a surface is fine; skipping it silently
is not.

Validation matches the surfaces you touched: the
[checks in `AGENTS.md`](../../../AGENTS.md), plus a live-provider smoke
through Doppler when the pass touched runtime behavior. Use isolated target
directories for `local-inference` and `cuda` work so the routine `target/`
stays clean. Do not declare release-ready for surfaces you did not actually
check.

## Report

- scope covered, and what was intentionally skipped with a reason
- what was fixed, and what was found
- evidence gathered: commands run, versions moved, sizes measured, tests and
  smoke results
- deferred findings with their GitHub issue numbers
- **blocked** if `main` CI is red and out of reach

If the user asks to ship the result, hand off to [`/ship`](../ship/SKILL.md).

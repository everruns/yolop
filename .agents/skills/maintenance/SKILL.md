---
name: maintenance
description: Goal-oriented repository maintenance and release-readiness work for yolop. Use when the user asks for maintenance, release prep, repo health review, dependency refreshes, spec/docs alignment, test gap review, or general cleanup without prescribing an exact sequence.
metadata:
  internal: true
user-invocable: true
---

# Maintenance

Goal: leave the repo materially healthier and closer to release-ready, with evidence.

This skill implements [`specs/maintenance.md`](../../../specs/maintenance.md). Keep operational guidance here. Keep design intent and constraints in the spec.

This skill is outcome-oriented. Choose the smallest set of actions that closes the real maintenance risk in front of you.

## When To Use

- release-readiness review
- CI health on `main`
- dependency refreshes (especially the `everruns-*` family)
- spec or docs drift
- feature-completeness drift across CLI / TUI / specs / README / tests
- test coverage gaps
- security hygiene review
- performance review of recently changed code
- AGENTS / skills / command hygiene

## Required Outcomes

1. **The maintenance scope is explicit.** If the user provided one, use it; otherwise state the inferred scope.
2. **The work produces concrete improvement.** Fix small/local issues; capture crisp findings for the rest.
3. **Validation matches risk.** Run checks that prove the updated areas are healthy.
4. **A release claim is backed by evidence.** Do not declare release-ready unless the changed surfaces were actually checked.

## Operating Model

- Start from goals and risk surface, not checklist order.
- A red CI on `main` outranks every other scope. Fix it first or open an issue and report the pass as blocked.
- Highest-signal first: recent diffs, failing checks, stale specs, outdated `everruns-*` versions.
- Skip untouched areas with a reason. Prefer fixing over reporting.
- For bugs uncovered, prefer a failing test before the fix when practical.
- Keep changes PR-sized. Defer anything larger to a GitHub issue and record the issue number in the report.

## Maintenance Surfaces

### CI Health

- check the latest workflow runs on `main` (`gh run list --branch main --limit 5`, through Doppler if GitHub auth fails directly)
- any red run is a hard gate: the pass is not complete while `main` is red
- if the failure is out of reach, open an issue with the failing run linked and report blocked

### Dependency Health

The `everruns-*` family (`everruns-runtime`, `-core`, `-anthropic`, `-openai`,
`-integrations-duckduckgo`) is the single most important dependency vector for
yolop. Keep them in lockstep at the same minor version.

Actions:
- check the latest version of each `everruns-*` crate (`cargo search everruns-runtime --limit 1`)
- bump them together — mixing minor versions is a soft API break
- run `cargo update` for transitive dependency drift
- check `ratatui`, `crossterm`, `clap`, `tokio` minors; they tend to ship breaking-feeling clippy/lint changes
- flag deprecated crates and identify replacements
- review `cargo tree --duplicates` for split transitive versions; fix or note why unfixable
- run `cargo audit` when available; otherwise check the repo's Dependabot alerts
- grep for direct dependencies no longer used in `src/`

Good evidence:
- `cargo build` + `cargo test --all-features` after bumps
- a successful smoke test (`doppler run -- cargo run -- --provider openai -p "hi"`) against at least one real provider

### Upstream Mirror Hygiene

Yolop began as `examples/coding-cli` in `everruns/everruns`. When upstream lands
useful changes:

- compare `src/` against the latest upstream example
- mirror non-everruns-specific improvements (UI tweaks, bug fixes, capability
  wiring) — leave behind anything tied to internal everruns paths
- record meaningful divergence as a comment near the diverged code, not a separate doc

### Specs And Docs Alignment

- changed behavior reflected in `specs/`, `README.md`, or `AGENTS.md`
- stale duplicate prose removed in favor of links to source files
- README provider and model lists match `runtime.rs`

### Feature Completeness Drift

A feature is ready only when its surfaces agree: CLI flags, TUI behavior,
`specs/`, `README.md`, tests, bundled `skills/`.

- diff `clap` definitions in `src/` against the README flag table
- check recently shipped features (see `git log` since the last tag) for a test that exercises them and a spec/README mention
- outcome: a small reconnecting fix, or a finding naming the missing surface and user-visible impact

### Code Simplification

On code touched during the pass:

- delete dead code, unreachable branches, commented-out blocks
- drop TODOs that are already resolved
- collapse premature generalization — code serves current needs, not hypothetical ones

### Security And Threat Posture

Yolop's threat surface is the host machine: filesystem and shell.

- verify the write blocklist in `runtime.rs` still covers `.git/`, `node_modules/`, `target/`, `dist/`, `build/`, `.next/`, `.venv/`, `venv/`, `.tox/`, `.gradle/`
- verify the bash tool still enforces a wall-clock timeout and per-stream output cap
- verify session JSONL log permissions stay at `0o600` on Unix
- confirm provider API keys are only read from process env, never logged or persisted to the session log

### Test And Runtime Confidence

- `cargo test --all-features` clean
- live integration test (`tests/integration.rs`) passes under Doppler
- offline smoke (`cargo run -- --provider llmsim -p "hi"`) prints a non-empty response and exits 0

## Common Evidence Commands

- `cargo fmt --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all-features`
- `cargo search everruns-runtime --limit 1`
- `cargo outdated` (when available)
- `cargo audit` (when available)
- `doppler run -- cargo run -- --provider openai -p "summarize this repo in one paragraph"` — live provider smoke
- `cargo run -- --provider llmsim -p "hi"` — offline smoke

## Deliverable

Report:

- what scope was covered
- what was fixed or found
- what evidence was gathered
- deferred findings, each with its GitHub issue number
- what was intentionally skipped and why
- **blocked** status if `main` CI is red and out of reach

If the user asks to ship after maintenance, hand off to [`/ship`](../ship/SKILL.md).

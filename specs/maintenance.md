# Maintenance Specification

## Abstract

This specification defines goal-oriented maintenance for yolop. Maintenance improves release readiness and repo health with evidence, not by mechanically executing a fixed checklist.

The canonical agent workflow lives in [`.agents/skills/maintenance/SKILL.md`](../.agents/skills/maintenance/SKILL.md). That skill is user-invocable so maintenance can be requested directly as `/maintenance`.

## Design Goals

1. Make the maintenance scope explicit.
2. Improve the repo in concrete ways or produce crisp findings with evidence.
3. Match validation depth to the actual risk surface.
4. Keep release claims honest.
5. Detect drift between yolop and its upstream source (`examples/coding-cli` in `everruns/everruns`).
6. Detect feature-completeness drift: features that look shipped on one surface (CLI flags, TUI behavior, specs, README, tests, bundled skills) but are missing or stale on another.

## Ownership Boundary

- This spec owns the maintenance intent, constraints, and success bar.
- The skill owns the execution workflow, heuristics, and example commands.

## Constraints

- Maintenance is risk-proportional, not sweep-proportional.
- The selected scope must be explained, including what was skipped and why.
- If maintenance changes code or behavior, affected artifacts must stay in sync: `README.md`, `AGENTS.md`, `specs/`.
- Maintenance prefers concrete fixes over ceremonial audits when a safe local fix exists.
- Dependency upgrades against external registries should respect a short release-age floor (≥1 day for patch, ≥7 days for minor/major) to avoid landing same-day yanks.

## CI Health Gate

GitHub Actions on `main` is the CI source of truth. The latest run on `main`
must be green before a maintenance pass is reported complete:

- A red `main` is the first maintenance item, ahead of any other scope.
- If the pass cannot fix the failure, it must open a tracked issue and report
  the pass as **blocked**, not complete.

## Deferred Findings

Findings too large to fix inline (multi-file refactors, upgrades needing
non-trivial rework) are deferred, not dropped:

- each deferred finding becomes a GitHub issue with scope and reproduction
- the issue numbers appear in the maintenance report

Deferred items are not failures. Untracked ones are.

## Feature Completeness Drift

A feature is not release-ready merely because one surface exists. Yolop's
surfaces are the CLI flags, the TUI behavior, `specs/`, `README.md`, the test
suite, and the bundled `skills/`. Maintenance should catch:

- flags or behavior present in `src/` but absent from `README.md` or `specs/`
- specs or README describing behavior the binary no longer has
- shipped features with no test exercising them

The outcome is either a small fix that reconnects the surfaces or a crisp
finding naming the missing surface and its user-visible impact — not a
generic "tech debt" note.

## Dependency Discipline

The `everruns-*` family is yolop's single most consequential dependency vector:

- `everruns-runtime`
- `everruns-core`
- `everruns-openai`
- `everruns-anthropic`
- `everruns-integrations-duckduckgo`

These crates ship together from one upstream workspace and are designed to be used at the same version. Yolop pins them at a single minor version. Mixing minor versions across the family is a soft API break and is not allowed without an explicit reason recorded in the PR.

Beyond the everruns family, dependency hygiene means:

- no known CVEs in the tree (`cargo audit` when available, plus the repo's Dependabot alerts)
- duplicate transitive versions reviewed (`cargo tree --duplicates`) — fix or note why unfixable
- no unused direct dependencies; prefer narrow sub-crates over umbrella crates when only a slice is used

## Upstream Mirror

Yolop began as `examples/coding-cli` in `everruns/everruns`. Maintenance should periodically:

- compare `src/` to the latest upstream example
- pull non-everruns-specific improvements (UI tweaks, bug fixes, capability wiring)
- leave behind anything tied to internal everruns paths or specs
- note material divergence as a comment near the code, not a separate doc

When upstream changes the public runtime API, bump the `everruns-*` versions in `Cargo.toml` together and reconcile any compile errors before the new feature lands.

## Release Readiness Standard

Before tagging a release:

- the `everruns-*` family is on the latest released minor
- `cargo build --release` succeeds and the resulting binary starts (`./target/release/yolop --help`)
- `cargo test --all-features` is green
- the live-provider integration test passes under Doppler
- the README's feature list, flag table, and provider env-var table match the source

## Security And Threat Posture

Yolop runs unsandboxed on the user's host. The threat surface is concentrated:

- **Filesystem** — the write blocklist in `runtime.rs` is the only thing preventing rewrites of `.git/`, dependency caches, and build artifacts. Maintenance must verify it is still wired through the real-disk file store.
- **Shell** — the bash tool spawns a real subprocess on the host. Maintenance must verify the timeout and the per-stream output cap.
- **Session log** — JSONL session logs contain prompts, tool arguments, and tool output. They must be created with `0o600` on Unix.
- **API keys** — provider keys must only be read from process env. They must never be written to the session log or echoed to tracing output.

## Spec Hygiene

Specs preserve design intent, rationale, and constraints — not implementation details readable from code. Maintenance should:

- replace duplicated struct/enum/field tables with links to source
- replace exhaustive feature-flag or capability lists with links to source
- keep the "why" and constraints; link to code for the "what"

## Related

- [`.agents/skills/maintenance/SKILL.md`](../.agents/skills/maintenance/SKILL.md)
- [`specs/shipping.md`](./shipping.md)

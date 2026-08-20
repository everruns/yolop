---
type: Process Specification
title: Maintenance Specification
description: Defines the success criteria for repository maintenance and release readiness.
---

# Maintenance Specification

## Abstract

This specification defines goal-oriented maintenance for yolop. Maintenance improves release readiness and repo health with evidence, not by mechanically executing a fixed checklist.

The canonical agent workflow lives in [`.agents/skills/maintenance/SKILL.md`](../../.agents/skills/maintenance/SKILL.md). That skill is user-invocable so maintenance can be requested directly as `/maintenance`.

## Design Goals

1. Make the maintenance scope explicit.
2. Improve the repo in concrete ways or produce crisp findings with evidence.
3. Match validation depth to the actual risk surface.
4. Keep release claims honest.
5. Detect drift between yolop and its upstream source (`examples/coding-cli` in `everruns/everruns`).
6. Detect feature-completeness drift: features that look shipped on one surface
   (CLI flags, TUI behavior, specs, README, docs, tests, bundled skills) but are
   missing or stale on another.
7. Reduce accidental complexity: remove over-abstraction, dead code, and premature generalization the codebase no longer earns.
8. Keep the shipped binary honest about its size: attribute growth to a cause before proposing a fix.

## Ownership Boundary

- This spec owns the maintenance intent, constraints, and success bar.
- The skill owns the execution workflow, heuristics, and example commands.

## Constraints

- Maintenance is risk-proportional, not sweep-proportional.
- The selected scope must be explained, including what was skipped and why.
- If maintenance changes code or behavior, affected artifacts must stay in sync:
  `README.md`, `docs/`, `AGENTS.md`, `knowledge/specs/`.
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
surfaces are the CLI flags, the presentation model, the TUI behavior, `--print`
output, ACP output, `knowledge/specs/`, `README.md`, `docs/`, the test suite, and bundled
system skills. Maintenance should catch:

- flags or behavior present in `src/` but absent from `README.md`, `docs/`, or
  `knowledge/specs/`
- specs or README describing behavior the binary no longer has
- shipped features with no test exercising them
- user-visible transcript/status behavior tested only through terminal buffers
  instead of the terminal-independent presentation model

The outcome is either a small fix that reconnects the surfaces or a crisp
finding naming the missing surface and its user-visible impact, not a
generic "tech debt" note.

## Dependency Discipline

The `everruns-*` family is yolop's single most consequential dependency vector:

- `everruns-runtime`
- `everruns-core`
- `everruns-platform`
- `everruns-openai`
- `everruns-anthropic`
- `everruns-integrations-duckduckgo`

These crates ship together from one upstream workspace and are designed to be used at the same version. Yolop pins them at a single minor version. Mixing minor versions across the family is a soft API break and is not allowed without an explicit reason recorded in the PR.

Beyond the everruns family, dependency hygiene means:

- no known CVEs in the tree (`cargo audit` when available, plus the repo's Dependabot alerts)
- duplicate transitive versions reviewed (`cargo tree --duplicates`), fix or note why unfixable
- no unused direct dependencies; prefer narrow sub-crates over umbrella crates when only a slice is used

## Binary Size

Yolop ships as a single binary, so its size is a user-visible cost three times
over: the release tarball download, the `cargo install yolop` build, and the
page-ins of every cold start. Size is a maintenance surface with the same
evidence bar as the others, a size claim needs a measured before and after on
one target and one profile, never an estimate.

[`cargo-bsize`](https://github.com/Boshen/cargo-bsize) is the tool of record. It
attributes the shipped bytes to crates, features, generic families, constant
data, and unwind tables, so a finding can name the thing to change instead of
only the number that hurts.

The shape of the binary decides which findings are worth chasing. Measured at
0.16.0 on `x86_64-unknown-linux-gnu` with default features, before the everruns
0.20 bump dropped the second HTTP/TLS stack, 79 MiB shipped of an 88.5 MiB
on-disk binary:

- About a third is tree-sitter parse tables. The 19 grammars are already shared
  by the symbol scan, ast-grep editing, and syntax highlighting, one copy each,
  so the only lever left is shipping fewer languages, a product decision rather
  than a cleanup.
- Yolop's own code is under 5%. Shrinking `src/` cannot move the total;
  dependency features and profile settings can.
- The remainder is dependency code and read-only data, so a size regression is
  usually a dependency bump or a feature-unification change, not a diff here.

Constraints:

- A lever that degrades a shipped guarantee is a decision, not a default. Two
  are rejected until the guarantee they break is rebuilt some other way:
  `panic = "abort"` drops the unwind and exception tables but breaks the crash
  path, where `join_worker` catches the worker thread's panic to print the
  crashed session id and report path before resuming the unwind; and
  `strip = "symbols"` leaves every crash-report backtrace frame as `<unknown>`,
  since a release build carries no debug info and the symbol table is all that
  names them.
- Levers that only trade build time for bytes are fair game, and their cost is
  paid by `cargo install` users too, not just by release CI.
- Bytes that a dependency's own feature choices force in cannot be fixed from
  here. Report them upstream with the measurement attached, and record the
  finding rather than working around it.

## Upstream Mirror

Yolop began as `examples/coding-cli` in `everruns/everruns`, but that example is
no longer a mirror source. In 0.17.24 upstream rebuilt it as the acceptance test
for the new public `everruns` facade: it depends only on that one crate, and a
test forbids it from touching `everruns-core` or `everruns-runtime`. Its TUI,
MCP, provider, and capability wiring were deleted. Yolop is now the more complete
agent of the two, so there is nothing left to pull from the example.

What remains worth tracking upstream is the **library surface**, not the example:

- the `everruns-*` crates yolop depends on, and their API changes
- the `everruns` facade's coverage, see the note beside the dependencies in
  `Cargo.toml` for why yolop does not use it yet, and revisit when it promotes
  provider registration, MCP, and capability wiring
- upstream's `CHANGELOG.md` highlights, which name the behavioral changes that a
  clean compile will not catch

When upstream changes the public runtime API, bump the `everruns-*` versions in
`Cargo.toml` together and reconcile any compile errors before the new feature
lands. A clean compile is not sufficient evidence of adoption: 0.17.24 widened
driver model discovery to include embedding models and began requiring HTTPS for
MCP OAuth resources, and 0.18.0 moved credentials out of model selection so a
host that keeps its own keys gets keyless drivers from the built-in provider
store. None of these showed up as a compile error.

Upstream also moves capability behind Cargo features, so read each cycle's
feature notes before assuming a default build still carries what it used to.
Host 0.20 made outbound A2A delegation opt-in behind the `everruns` crate's
`a2a` feature: yolop does not delegate to remote A2A agents, so it stays off and
the default build no longer pulls a second HTTP/TLS stack. Enable it only if
yolop grows an outbound A2A path.

## Release Readiness Standard

Before tagging a release:

- the `everruns-*` family is on the latest released minor
- `cargo build --release` succeeds and the resulting binary starts (`./target/release/yolop --help`)
- `cargo test --workspace --features yolop-yep/schema` is green
- the live-provider integration test passes under Doppler
- the README's feature list, flag table, and provider env-var table match the source

## Security And Threat Posture

Yolop uses native containment for arbitrary shell commands by default. The
remaining threat surface is concentrated:

- **Filesystem**: structured file tools still use the rooted real-disk broker
  and protected-path checks. Maintenance must verify those mounts and checks
  remain wired.
- **Shell**: arbitrary commands spawn through the configured sandbox provider.
  Maintenance must run the workspace-write, outside-write, network-denial,
  worktree-switch, timeout, and output-cap tests on macOS and Linux.
- **Session log**: JSONL session logs contain prompts, tool arguments, and tool output. They must be created with `0o600` on Unix.
- **API keys**: provider keys must only be read from process env. They must never be written to the session log or echoed to tracing output.

[`sandboxing.md`](./sandboxing.md) defines the implemented boundary and its known limitations.

## Code Simplification And De-Abstraction

Complexity accretes: an abstraction added for a second caller that never
arrived, a trait with one impl, a config knob nobody sets. A deep maintenance
pass treats removing that complexity as real work, not a side effect. The bias
is toward deletion, the healthiest passes often remove more code than they add.

Maintenance should look for and collapse:

- single-use abstractions (one-impl traits, forwarding wrappers, single-instantiation generics, builders for trivial structs), unless the boundary is essential
- premature generalization: flexibility shaped for hypothetical futures, not current callers
- indirection with no payoff: helpers that only rename a stdlib call, modules that re-export one item, always-default knobs
- under-abstraction: the same block pasted in several places, where a shared helper genuinely reduces total code
- deep nesting and long match arms that a flatten or extraction makes legible
- names that hide intent

Constraints:

- A simplification must preserve behavior. It is verified by build, clippy, and
  the test suite, a behavior change disguised as cleanup is a regression.
- Keep simplifications small and independently reviewable; do not fold a
  de-abstraction sweep into an unrelated change.
- Removing a public item from the published `yolop-yep` crate is a breaking
  change and must be called out, not slipped in.
- A simplification too large to land inline (a cross-cutting abstraction with
  many call sites) is deferred to a tracked issue naming the abstraction and why
  it no longer pays its way, same discipline as any other deferred finding.

This is the inverse of premature abstraction, not an argument against all
abstraction: an abstraction that carries real, current weight stays.

## Spec Hygiene

Specs preserve design intent, rationale, and constraints, not implementation details readable from code. Maintenance should:

- replace duplicated struct/enum/field tables with links to source
- replace exhaustive feature-flag or capability lists with links to source
- keep the "why" and constraints; link to code for the "what"

## Agent Context Hygiene

The context agents read drifts the same way code does, and duplication is its
characteristic failure: a rule stated in `AGENTS.md`, restated in a spec, and
restated again in a skill will disagree within a few changes. Maintenance should
check the layering defined in [`agent-context.md`](./agent-context.md), one
owner per rule, `AGENTS.md` carrying only every-turn facts, skills thin at the
top with reference material split out, and no instruction that both mandates a
step and invites judgment about it.

## Related

- [`.agents/skills/maintenance/SKILL.md`](../../.agents/skills/maintenance/SKILL.md)
- [`knowledge/specs/agent-context.md`](./agent-context.md)
- [`knowledge/specs/documentation.md`](./documentation.md)
- [`knowledge/specs/shipping.md`](./shipping.md)

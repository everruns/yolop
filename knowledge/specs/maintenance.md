---
type: Process Specification
title: Maintenance Specification
description: Defines the success bar for goal-oriented repository maintenance and release readiness.
---

# Maintenance

## Abstract

A maintenance pass keeps yolop healthy, honest, and ready to ship. The pass
reviews the repository as a whole, fixes what is small and local, and records
anything larger as a tracked issue with its user-visible impact. The goal is a
repository that stays releasable: green CI on `main`, exact and current
upstream dependencies, a clean toolkit boundary, a compatible extension wire,
a documented binary size, complete features across every surface, and specs
that describe what the code does.

This spec owns the success bar and the rationale. The
[`maintenance` skill](../../.agents/skills/maintenance/SKILL.md) owns how to
work a pass, and
[`surfaces.md`](../../.agents/skills/maintenance/surfaces.md) owns the
per-surface commands and heuristics.

## Design Goals

1. Leave the repo materially healthier than the pass found it, with evidence.
2. Keep `main` releasable: green CI, honest version numbers, and a tested
   release path.
3. Fix root causes inside the pass. A large diff, a long build, a missing
   audit, or an unfamiliar surface is work to complete, not a reason to defer.
4. Never weaken tests, security limits, or supply chain criteria to ship an
   upgrade.
5. Track the upstream library and toolkit surfaces for behavioral changes,
   even when the code still compiles.
6. Tie each surface back to the users who feel it: startup time, binary size,
   terminal rendering, extension compat, agent loop quality, and docs accuracy.
7. Make release readiness a verdict with evidence, not a feeling.

## Ownership Boundary

This spec owns the success bar and the rationale behind each surface. The
`maintenance` skill owns how to work a pass: scope, order, validation, and
report shape. `surfaces.md` owns the per-surface commands and heuristics.
When a surface grows a new command, update `surfaces.md`. When the bar itself
changes, update this spec.

## Constraints

- A patch release less than one day old is too fresh for a routine pass.
  Minor and major Everruns releases can be adopted immediately after their
  compatibility review. Critical security fixes are exempt.
- Keep a single routine feature set per `target/` directory. The schema
  feature (`--features yolop-yep/schema`) resolves to the same crates a
  default build does. `--all-features` also turns on `local-inference` and
  its large engine graph, and mixing the two in one `target/` compiles the
  whole graph twice. Anything behind `local-inference` or `cuda` gets its own
  `CARGO_TARGET_DIR` per `AGENTS.md`.
- Risk proportional upgrades. Patch and minor bumps that preserve the
  `everruns` and `tuika` contracts go through the pass. Major bumps, facade
  changes, protocol version changes, and distribution changes ship as their
  own tested PRs. A major inside the refresh scope gets a real evaluation,
  not a silent skip.
- Synchronize artifacts with code: specs, README flags and tables, provider
  and model lists, extension manifests, the OpenAPI surface where one exists,
  threat posture notes, test cases, and agent instructions. A behavior change
  with only code updated is incomplete.
- Never weaken a test, a security limit, or a supply chain criterion to make
  an upgrade fit. If the newest upstream version cannot preserve a required
  contract, establish the incompatibility, retain the newest safe version with
  a tested and documented pin, and complete the rest of the pass.

## CI Health Gate

A red CI on `main` outranks every other maintenance scope. The pass fixes it
first, or opens an issue and reports **blocked**.

Rationale: `main` is the release source. Feature, dependency, docs, and size
work built on a red `main` cannot be validated. The gate keeps the pass from
producing changes that look healthy but were never proven against a green
baseline.

## Deferred Findings

A finding that is too large to fix inline, for example a refactor that spans
several subsystems or a story that needs design discussion, is deferred to a
GitHub issue naming the problem and its user-visible impact. The pass records
the issue number in its report.

Rationale: maintenance is time boxed. Issues preserve a crisp problem
statement so work survives the pass. The user-visible impact keeps the backlog
prioritized by what users feel, not by what is architecturally elegant.

Deferral is for genuine scope, not for avoiding work. A large diff, a long
build, a missing audit, or an unfamiliar failure is work to diagnose and
finish inside the pass. Only a true external blocker, a missing permission, an
unavailable service, or an explicitly requested deferred scope justifies
stopping short.

## Release Readiness Standard

A release readiness verdict covers the release path in
[`release.md`](release.md) and the merge bar in
[`shipping.md`](shipping.md), scoped to what the pass actually checked:

- `main` CI is green, including the wire schema drift guard with
  `--features yolop-yep/schema`.
- The `everruns-*` family and the `tuika` family are current, with exact
  pins that still match `Cargo.lock`.
- Versions agree: the root crate, `yolop-yep`, `Cargo.lock`, and the
  first-party extension manifests. The extension manifest pin test proves the
  agreement.
- The publish order still holds: `yolop-yep` first, then the binary, then the
  extensions.
- The release build starts, per the release spec, and the binary size section
  below has fresh numbers against a stated baseline.
- Terminal verification tiers for the changed UI surfaces are green: the
  tuika upstream gates and the human walk where the tuika spec requires them.
- Docs that gate a release still match: provider and model lists, flag
  tables, and the README scope statement.

Rationale: readiness rots between releases. Dependency drift, forgotten
manifest bumps, stale tables, and untested UI paths accumulate silently. The
checklist forces the pass to re-prove the path instead of assuming the last
release still works.

Do not declare release-ready for surfaces the pass did not actually check.

## Dependency and Toolchain Health

- `everruns-host` and every `everruns-*` provider crate stay on exact version
  pins. The graph is large and transitive drift is the failure mode the pins
  prevent.
- Reviews consider the whole published family (`everruns-host`,
  `everruns-core`, `everruns-anthropic`, `everruns-openai`,
  `everruns-integrations-duckduckgo`, `everruns-platform`), not only the
  crates named in the root manifest, since a stale transitive member can keep
  a known bug alive.
- Patch and minor updates go through `cargo update -p <crate>` deliberately.
  Prefer `cargo update --dry-run` for
  review, and re-resolve from scratch (`rm Cargo.lock`, fresh `cargo update`,
  rebuild) to prove the manifest pins are sufficient before restoring the
  lockfile if the experiment fails.
- Updates must leave `cargo metadata --locked` clean, with no yanked crates,
  no `cargo audit` findings when the tool is available, no duplicate crate
  versions, and no unused dependencies.
- The Rust toolchain stays current and the minimum supported Rust version in
  `rust-toolchain.toml`, CI, and docs stays synchronized. Version drift
  between those three is itself a finding.
- Major upgrades ship as their own PRs with the breaking changes reviewed.
  A major refresh scope goes deep: read the upstream breaking notes, assess
  migration cost against what the new version buys, and land the upgrade or
  record why the pin stays with a re-check trigger. Skipping a major because
  it looks painful is not a verdict. Transitive runtime dependencies that
  must move together move together.

Rationale: yolop is downstream of a fast moving library family. Exact pins
make builds reproducible, and deliberate updates make behavior changes
reviewable. Stale transitive members and yanked crates are the quiet ways a
green build keeps a known bug.

## Upstream Library Surface

- Read the upstream `CHANGELOG.md` files for the `everruns-*` crates in the
  refresh scope. Treat behavioral notes as findings even when the code
  compiles cleanly: retry, timeout, tool calling, MCP, session, and provider
  changes alter the agent loop without breaking the build.
- Check facade coverage. When upstream adds a new facade capability the agent
  loop or a provider driver should use, the gap is a finding. When upstream
  adds a feature gated capability (for example `a2a` or a local backend), the
  pass decides explicitly whether yolop opts in or records why it stays out.
- When a library introduces a new thing or a new paradigm, for example a new
  provider capability, a new extension pattern, or a new TUI idiom, the pass
  evaluates what adopting it would simplify or unlock, prototypes where the
  claim is uncertain, and records an explicit adopt or decline with reasons.
- Check the minimum supported Rust version and the lockstep between related
  crates. What is published on crates.io wins over local habit.

Rationale: the compiler only proves the call shapes still match. Upstream
behavioral changes, new capabilities, and requirement changes pass through a
clean build and surface as agent regressions, provider failures, or release
surprises. The changelog is the only cheap detector.

## Tuika Boundary

Toolkit-shaped work, including layout, components, overlays, focus, keymap,
markdown rendering, terminal escapes, and screen modes, belongs in the
`tuika` repository, not here. What belongs here is how yolop composes it.

- The `tuika`, `tuika-codeformatters`, and `tuika-mermaid` versions stay
  current, move together where they are
  companion releases, and keep `Cargo.toml` and `Cargo.lock` in agreement.
  No path or git dependencies: a git dependency would make yolop
  unpublishable.
- Read the upstream `tuika` changelog for rendering, input, and escape
  changes that compile clean but alter the TUI. The tuika upstream gates are
  the detectors.
- A needed toolkit change lands upstream first, releases, then bumps here.
  A local workaround that belongs upstream is a finding with an upstream
  issue, not a permanent local shortcut.

Rationale: the TUI is the product surface users see first. A stale toolkit
keeps rendering bugs alive, and a local toolkit shaped shortcut forks the
architecture. The boundary keeps fixes where they compound.

## YEP Wire Compat

- The wire schema drift guard stays green with
  `--features yolop-yep/schema`. A schema change without a regenerated
  artifact is a finding.
- The protocol version, the `yolop-yep` crate version, and the first-party
  extension manifests stay in agreement. The manifest pin test that asserts
  `plugin.json` matches `Cargo.toml` is the proof.
- Extension servers in the matrix still enumerate, start, use tools and
  resources, and render their UI affordances per the extensions spec. A
  protocol bump ships as its own tested PR with the publish order preserved.

Rationale: the extension wire is a compat promise to third party authors.
Silent drift breaks their servers while yolop stays green. The drift guard
and the manifest pin test are the only automated witnesses.

## Local-Inference, Metal, CUDA Matrix

Code behind `local-inference` (`src/drivers/local.rs`, `src/models/`) and the
accelerated release builds (`metal` on macOS, `cuda` on Linux) is outside the
routine feature set. The pass touches it only when the scope includes it, and
then with the commands in `AGENTS.md`:

- `local-inference` checks run under their own `CARGO_TARGET_DIR` so the
  large engine graph never pollutes the routine target directory.
- A `cuda` change also wants the backend compiled with `nvcc` and
  `CUDA_COMPUTE_CAP` set, since kernels target one capability and no GPU
  exists here to query.
- Distribution stays split per the local inference spec: the default
  portable build never absorbs the engine, and the accelerated builds stay
  per target.

Rationale: the engine graph is hundreds of crates. Compiling it inside the
routine target directory wastes every later routine command, and an untested
accelerated build ships a release artifact nobody proved.

## Binary Size

The pass measures the release binary and reports the total plus the component
breakdown (binary count where applicable, largest dependencies, feature
contributions) against a stated baseline: the previous release tag for a
release readiness scope, otherwise the last recorded maintenance numbers.
Unexplained growth above noise, roughly 5 percent or 5 MB, is a finding with
an owner.

Rationale: yolop ships a single binary and size regressions are silent.
Dependency additions, new backends, and debug settings accumulate without any
test turning red. Evidence keeps the conversation about tradeoffs instead of
surprise.

## Feature Completeness Drift

When the pass covers a behavior, it walks every surface that behavior touches:
CLI flags, the fullscreen TUI, `--inline` mode, print mode, ACP, skills,
README, `docs/`, specs, and tests. The worst drift is a flag that works in
one mode but is missing, stale, or contradictory in another.

Every user-facing terminal state, including success, failure, working,
awaiting approval, offline, and diff views, stays reachable in the gallery
wiring so its ratatui buffer diff has nowhere to hide a wrong style or a
broken layout. Every status and every new behavior carries a
behavior-anchored test with an explicit transcript or status line assertion,
full screen or inline as appropriate. The presentation model and the terminal
buffers are the authoritative contract, so specs describe that model and tests
assert it.

Rationale: yolop presents the same run through several renderers. A behavior
added to one surface and forgotten in the others reads as a broken product.
Buffer asserted tests are the only check that survives refactors of the view
layer.

## Test and Runtime Confidence

- The routine suite runs with the schema feature:
  `cargo test --workspace --features yolop-yep/schema`, after `cargo fmt`
  and `cargo clippy` per `AGENTS.md`. The pass never mixes feature sets in
  one target directory.
- The offline smoke runs without keys: `--provider llmsim`. A runtime behavior
  change also gets a live provider smoke through Doppler. Tests that need
  something the environment may lack check at runtime and return early (a key
  via `live_key_or_skip`, a binary via a probe, an external service via
  `YOLOP_REQUIRE_LIVE_TESTS`); ignored tests are forbidden.
- No PTY test remains: the gallery demo that carried it is gone, and terminal
  protocol coverage lives in the tuika upstream gates. Flag any new
  provider-free fullscreen mode as needing a replacement witness.
- `evals/` holds Mira studies outside the Cargo workspace. The pass runs them
  only when the scope touches prompt or tool behavior and the scope asks for
  it. Otherwise the pass notes them as skipped with that reason. A prompt or
  tool behavior with no eval covering it is a finding: propose the missing
  eval or record why the behavior needs none.
- Review the tests themselves, not only their pass rate. Coverage gaps on
  changed behavior are findings. So are tests that prove nothing obvious,
  duplicated tests that assert the same behavior twice, slow or flaky or
  over-mocked tests that cost more than they protect, and nonsense tests
  whose assertions cannot fail or do not match their names. Fix, merge, or
  delete; never weaken an assertion to make a suite green.

Rationale: unit tests prove logic, the smoke tests prove the provider wiring
works with and without keys, and evals prove the agent still accomplishes
work. Terminal protocol proof lives in the tuika upstream gates. Each layer
catches what the others cannot.

## Security and Threat Posture

- The sandbox spec owns the trust boundary. The pass re-checks the filesystem
  broker and command execution mounts, the deny by default posture, and the
  rule that sandbox shaped logic lives behind the provider trait, not in
  callers.
- Shell provider changes run the provider tests on both macOS and Linux.
  Platform specific quoting, Robertson versus Bourne differences, and ACL
  versus mode behavior diverge silently.
- Session logs stay owner read write only, API keys stay environment only,
  and link rendering stays opt-in per the shipping security review.
- A scope that touches execution, filesystem access, secrets, or rendering
  of untrusted content gets a structured security review from the ship
  skill, not only a green build. The pass names what was reviewed and what
  the review concluded.

Rationale: yolop executes untrusted model output as shell commands and file
operations. The threat posture is the product. A convenience shortcut in a
caller, a permissive mount, or a logged secret is a vulnerability, not a
refactor.

## Simplicity, Soundness, and Architecture

Prefer deleting code over adding it. Consolidate special cases into general
mechanisms. Remove one-off helpers, merge overlapping abstractions, and
simplify control flow while keeping every test green. Each simplification
ships as its own independently reviewable change.

Check soundness where the pass looks: invariants that callers must hold but
nothing enforces, error paths that swallow context, and concurrency or
ordering assumptions the code never states. Check architecture where the
pass looks: module boundaries that leak, layering that inverts, and new code
that duplicates an existing mechanism under a new name. A shortcut with no
owner and no removal trigger is a finding.

Rationale: unneeded abstraction is where bugs hide, and unstated invariants
are where outages hide. A smaller, more direct codebase with explicit
boundaries is easier to review, easier to test, and less likely to drift
from its specs. Simplification compounds across passes.

## Spec Hygiene

- Specs record what is true and why. Update specs when behavior, intent,
  architecture, policy, constraints, or terminology changes. Transient plans
  and source level detail stay out.
- Knowledge and code must agree. A spec that contradicts the code is a bug
  in exactly one of the two: fix the side that is wrong and say which side
  changed. The pass spot checks specs against behavior for every surface it
  covers instead of assuming they match.
- Replace duplicated tables and lists with links to their owning source.
  When a README table must stand alone for offline readers, say so next to it
  and keep the sync obligation explicit.
- `knowledge/index.md` changes when concepts are added, removed, renamed, or
  reclassified. `knowledge/log.md` records significant knowledge changes
  under `DATE, Title` headings.
- Knowledge changes run `python3 scripts/validate_okf.py knowledge
  --check-links`, and any concept that contradicts another concept names the
  winner inline.

Rationale: specs are the durable memory. Duplicated facts rot, unlinked
concepts vanish from discovery, and an unlogged intent change looks like an
accident to the next reader.

## Docs Contract

- `README.md` and `docs/` never link into `knowledge/` or `.agents/`. They
  are the public surface; the bundle is the working memory.
- Provider and model lists, flag tables, and setup instructions match the
  code that owns them (`runtime.rs`, the CLI definition, the extension
  manifests). A table that disagrees with its source is a finding.
- The README stays sound: its scope claim, install path, and quickstart
  reflect the current product, and its quickstart commands actually run. A
  claim the product no longer keeps is a finding, not a footnote.
- Docs stay current with behavior. A doc page that describes removed flags,
  renamed commands, or past architecture is a finding with an owner, whether
  the fix is an update or a deletion.
- Prose across `knowledge/`, docs, commit messages, and PR bodies avoids
  em-dashes. A comma, colon, or separate sentence says the same thing.
- Visual claims about the TUI carry fresh captures from the real TUI or the
  tuika upstream evidence where the docs spec requires them.

Rationale: docs are a release artifact. A stale flag table or a phantom
model name breaks user trust faster than a bug, because users stop believing
the rest of the page.

## Agent Context Hygiene

- Resolve the duplication: each rule has exactly one owner. If a rule appears
  in both `AGENTS.md` and a knowledge spec, that is a bug in exactly one of
  the two.
- `AGENTS.md` stays entry level: facts and gotchas with links. Depth moves to
  the knowledge specs it points at.
- Skills own workflows, specs own success bars. A skill that redefines the
  bar and a spec that prescribes keystrokes are both findings.

Rationale: agents read `AGENTS.md` every turn. Duplicated or drifting
instructions waste attention and produce confident mistakes. One owner per
rule keeps the context cheap and correct.

## Reporting Standard

The pass report states:

- The scope covered, and what was intentionally skipped with a reason.
- What was fixed and what was found.
- The evidence gathered: commands run, versions moved, sizes measured,
  tests and smoke results.
- Deferred findings with their GitHub issue numbers.
- **Blocked** when `main` CI is red and out of reach.

If the user asks to ship the result, hand off to `/ship`. Maintenance proves
health; shipping owns the merge bar.

## Frequency

Run a pass before each release, when upstream `everruns-*` or `tuika` drift is
suspected, when binary size moves without explanation, when docs or specs
drift from behavior, or when CI on `main` needs a health verdict. The scope
follows the signal: recent diffs, failing checks, stale knowledge, and
outdated upstream versions first.

## Related

- [`shipping.md`](shipping.md), the merge bar and release safety checks.
- [`release.md`](release.md), the release path, publish order, and terminal
  verification.
- [`tuika.md`](tuika.md), the toolkit boundary and verification workflow.
- [`extensions.md`](extensions.md), the YEP wire, manifest pins, and server
  matrix.
- [`local-inference.md`](local-inference.md), the engine boundary and
  distribution split.
- [`sandboxing.md`](sandboxing.md), the trust boundary and threat posture.
- [`documentation.md`](documentation.md), the public surface contract.
- [`agent-context.md`](agent-context.md), how agent context is organized.

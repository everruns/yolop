# Maintenance surfaces

Per-surface commands and heuristics. Open only the surfaces the scope covers.
The [maintenance spec](../../../knowledge/specs/maintenance.md) owns the bar;
this file owns how to check it.

## CI health on `main`

```bash
gh run list --branch main --limit 10
gh run view <failing-run-id> --log-failed
```

A red `main` outranks everything. Fix it first or report **blocked**. Common
causes: a stale lockfile, a yanked crate, an upstream behavior change that
compiles clean, or a TUI regression.

## Dependency and toolchain health

```bash
cargo update --dry-run
cargo update -p <crate>
cargo metadata --locked
cargo audit  # when the tool is available
cargo fmt --check
cargo clippy --workspace --all-targets --features yolop-yep/schema -- -D warnings
cargo test --workspace --features yolop-yep/schema
```

Heuristics: review the whole `everruns-*` family, not only the crates named
in the root manifest (`everruns-host`, `everruns-core`,
`everruns-anthropic`, `everruns-openai`,
`everruns-integrations-duckduckgo`, `everruns-platform`). Respect the
release age floor: under one day for patches, under seven days for minor and
major. Re-resolve from scratch to prove manifest pins are sufficient before
restoring the lockfile on failure. End state is `cargo metadata --locked`
clean, no yanked crates, no audit findings where the tool runs, no duplicate
versions, no unused dependencies, and `rust-toolchain.toml`, CI, and docs in
agreement. Major upgrades and protocol bumps ship as their own PRs. Go deep
on majors in scope: read the breaking notes, weigh migration cost against the
gain, and land the upgrade or record why the pin stays with a re-check
trigger. When a library introduces a new thing or a new paradigm, evaluate
adoption explicitly: what it simplifies or unlocks, with a prototype where
the claim is uncertain.

## Upstream library surface

Read the upstream `CHANGELOG.md` files for the `everruns-*` crates in scope.
Treat behavioral notes as findings even when the build is green: retry,
timeout, tool calling, MCP, session, and provider changes alter the agent
loop without breaking call shapes. Check facade coverage for new upstream
capabilities the loop or a driver should adopt, and decide explicitly on
feature gated additions such as `a2a` or local backends. When a release
introduces a new paradigm, evaluate what adopting it would simplify or
unlock, and record an explicit adopt or decline with reasons. What is
published on crates.io wins.

## Tuika boundary

```bash
cargo update --dry-run -p tuika -p tuika-codeformatters -p tuika-mermaid
```

Heuristics: the three versions move together where they are companion
releases, `Cargo.toml` and `Cargo.lock` agree, and no path or git dependency
exists. Read the upstream changelog for rendering, input, and escape changes
that compile clean. Prove TUI changes with unit coverage and the tuika
upstream gates where the tuika spec requires it. Toolkit shaped fixes belong
upstream first, then release, then bump here.

## YEP wire compat

```bash
cargo test --workspace --features yolop-yep/schema
```

Heuristics: the schema drift guard is the first witness. A schema change
without a regenerated artifact is a finding. The protocol version, the
`yolop-yep` crate version, and the first-party extension manifests agree,
proven by the manifest pin test asserting `plugin.json` matches `Cargo.toml`.
Exercise the extension server matrix (enumerate, start, tools, resources, UI
affordances) when the wire moved. Publish order holds: `yolop-yep` first,
then the binary, then the extensions.

## Local inference, metal, CUDA matrix

Only when the scope includes it. Keep every routine command on the schema
feature set in the default `target/`; give the engine and the backends their
own directories per `AGENTS.md`:

```bash
CARGO_TARGET_DIR=target-local-inference \
  cargo clippy --workspace --all-targets --features local-inference -- -D warnings
CARGO_TARGET_DIR=target-cuda CUDA_COMPUTE_CAP=80 \
  cargo check -p yolop --locked --features cuda
```

`cuda` needs `nvcc` installed. Distribution stays split: the default portable
build never absorbs the engine, accelerated builds stay per target.

## Knowledge and docs alignment

```bash
python3 scripts/validate_okf.py knowledge --check-links
```

Heuristics: specs record what is true and why; replace duplicated tables
with links to the owning source. Spot check specs against code on every
covered surface: a contradiction is a bug in exactly one side, fix the wrong
side and say which changed. `README.md` and `docs/` never link into
`knowledge/` or `.agents/`. Verify the README scope claim, install path, and
quickstart by running the commands; stale pages describing removed flags or
past architecture are findings with owners. Provider and model lists match `runtime.rs`,
flag tables match the CLI definition, manifest fields match the extensions
spec. Prose avoids em-dashes. Update `knowledge/index.md` when concepts are
added, removed, renamed, or reclassified, and `knowledge/log.md` for
significant changes under `DATE, Title` headings.

## Feature-completeness drift

Walk every surface a behavior touches: CLI flags, fullscreen TUI,
`--inline`, print mode, ACP, skills, README, `docs/`, specs, and tests. Check
recent diffs for behaviors added to one renderer and forgotten in another.
Every user-facing terminal state stays reachable in the real TUI, and
every status or new behavior carries a behavior anchored test with an
explicit transcript or status line assertion.

## Code simplicity, soundness, architecture

Look for one-off helpers, overlapping abstractions, and special cases that a
general mechanism already covers. Delete before adding. Keep each
simplification independently reviewable with tests green. Check soundness:
unenforced invariants, swallowed error context, unstated ordering
assumptions. Check architecture: leaking module boundaries, inverted
layering, new code duplicating an existing mechanism under a new name.

## Binary size

Measure the release binary with a stated baseline: the previous release tag
for release readiness, otherwise the last recorded maintenance numbers.
Report the total plus the component breakdown. Unexplained growth above
noise, roughly 5 percent or 5 MB, is a finding with an owner. Suspects
include new backends, new default features, debug settings, and duplicated
native libraries.

## Security posture

Re-check the sandbox trust boundary: filesystem broker and command execution
mounts, deny by default, sandbox logic behind the provider trait. Shell
provider changes run the provider tests on both macOS and Linux. Session logs
stay owner read write only, API keys stay environment only, link rendering
stays opt-in. Scopes touching execution, filesystem access, secrets, or
untrusted rendering get the structured security review from the ship skill,
and the report names what was reviewed and what it concluded. When in doubt,
run that review rather than asserting safety from a green build.

## Test and runtime confidence

```bash
cargo run -- --provider llmsim -p "hi"
doppler run -- cargo run -- --provider openai -p "hi"
```

Heuristics: the offline `llmsim` smoke always runs when runtime behavior
moved; the Doppler live smoke follows for provider wiring. Tests that need
something the environment may lack check at runtime and return early; ignored
tests are forbidden. Review the suite itself: coverage gaps on changed
behavior, tests that prove nothing obvious, duplicates asserting the same
behavior twice, slow or flaky or over-mocked tests that cost more than they
protect, and nonsense tests whose assertions cannot fail. `evals/` studies run
only when the scope touches prompt or tool behavior and asks for them,
otherwise note them as skipped with that reason; a behavior with no covering
eval is a finding, propose the missing eval or record why none is needed.

## Release readiness

Check only what the scope covered, and say what was not checked. For a
release readiness scope: `main` green with the schema feature, upstream
families current within the floor with pins matching the lockfile, versions
in agreement across the root crate, `yolop-yep`, lockfile, and extension
manifests, publish order intact, the release build starting per the release
spec, fresh binary numbers, green terminal verification tiers for changed UI,
and docs tables matching their sources.

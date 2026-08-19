# Maintenance surfaces — commands and heuristics

Operational companion to [`SKILL.md`](SKILL.md). Why each surface matters is in
[`knowledge/specs/maintenance.md`](../../../knowledge/specs/maintenance.md); this file is what to run.

## CI health

```bash
gh run list --branch main --limit 5   # through Doppler if GitHub auth fails
```

Any red run is a hard gate. If the failure is out of reach, open an issue with
the failing run linked and report the pass blocked.

## Dependency health

The `everruns-*` family (`-runtime`, `-core`, `-anthropic`, `-openai`,
`-integrations-duckduckgo`) moves in lockstep at one minor version.

```bash
cargo search everruns-runtime --limit 1
cargo update                    # transitive drift
cargo tree --duplicates         # split transitive versions: fix or explain
cargo audit                     # when available; otherwise Dependabot alerts
```

Also check `ratatui`, `crossterm`, `clap`, and `tokio` minors — they tend to
ship breaking-feeling lint changes. Grep for direct dependencies no longer used
in `src/`, and flag deprecated crates with a replacement.

Evidence after a bump: `cargo test --features local-inference`, plus one real-provider
smoke (`doppler run -- cargo run -- --provider openai -p "hi"`).

## Upstream mirror

Compare `src/` against the current `examples/coding-cli` in `everruns/everruns`.
Mirror improvements that are not tied to internal everruns paths; record
meaningful divergence as a comment next to the diverged code.

## Knowledge and docs alignment

```bash
python3 scripts/validate_okf.py knowledge --check-links
```

Read `knowledge/index.md`, then inspect the concepts touched by behavior that
changed since the last pass. Staleness means contradiction or missing coverage,
not age. Confirm the index covers and correctly classifies every concept, mark
superseded concepts, and log significant changes in `knowledge/log.md`.

Check `AGENTS.md`, `README.md`, and `docs/` against
[`knowledge/specs/agent-context.md`](../../../knowledge/specs/agent-context.md) and
[`documentation.md`](../../../knowledge/specs/documentation.md): no rule owned in two places, no public
page linking into `knowledge/` or `.agents/`, README provider and model lists
matching `runtime.rs`.

## Feature-completeness drift

Diff the `clap` definitions in `src/` against the README flag table. For
features shipped since the last tag (`git log`), confirm each has a test that
exercises it and a knowledge/README mention. The outcome is a small reconnecting
fix, or a finding naming the missing surface and its user-visible impact.

## Simplification and de-abstraction

On code touched during the pass: delete dead code, unreachable branches,
commented-out blocks, and resolved TODOs.

On a deep pass, hunt for complexity the codebase no longer earns — single-use
abstractions, premature generalization, indirection with no payoff, duplication
that wants a helper, deep nesting, names that hide intent. Verify with `cargo
build`, `cargo clippy`, and `cargo test`: a simplification that changes behavior
is a bug. Removing a public item from `yolop-yep` is a breaking
change — call it out in the PR.

## Security posture

- the write blocklist in `runtime.rs` still covers `.git/`, `node_modules/`,
  `target/`, `dist/`, `build/`, `.next/`, `.venv/`, `venv/`, `.tox/`, `.gradle/`
- the bash tool still enforces a wall-clock timeout and per-stream output cap
- session JSONL log permissions stay `0o600` on Unix
- provider keys are read from process env only — never logged or persisted

## Test and runtime confidence

```bash
cargo test --features local-inference
doppler run -- cargo test --features local-inference --test integration
cargo run -- --provider llmsim -p "hi"     # non-empty response, exit 0
```

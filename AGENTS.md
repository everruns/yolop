# Yolop — coding-agent guidance

Yolop is a terminal coding agent built on top of
[`everruns-runtime`](https://crates.io/crates/everruns-runtime). The binary is
named `yolop`; the crate is `yolop`.

This file is read on every turn by the agent itself when run inside this
repository, so keep it short, factual, and project-specific.

## Workflow

- Telegraph. Drop filler. Keep updates short and factual.
- Fix the root cause. If unsure, read more code; if still stuck, ask with short options.
- When `repo_map` or `repo_symbols` reveals a likely owning module, narrow to that module next. Read it and one or two call sites/tests before doing more broad grep; resume broad search only if the owner is disproven.
- Unrecognized working-tree changes are probably from another agent or the user. Work with them. Stop only if they make the task unsafe.
- Start from latest `main` by default: `git fetch origin main`, then branch from or rebase onto `origin/main`.
- Keep changes small, PR-sized, testable, and runnable locally.
- For bug fixes, write or update a failing test before the fix when practical.
- Important decisions belong as concise comments near the relevant code, not in scratch docs.
- No backward compatibility required unless a spec says so — `yolop` is pre-1.0.

## Cloud and secrets

Use Doppler for any secret-backed command. `OPENAI_API_KEY` is the default
provider key; `ANTHROPIC_API_KEY` is the secondary. CI loads both via the
`DOPPLER_TOKEN` repository secret.

```bash
doppler run -- cargo test --all-features
doppler run -- cargo run -- -p "say hi"
```

Use `gh` directly first. Only if `gh` reports that it is not authenticated, retry
through Doppler; do not invoke Doppler preemptively for GitHub commands:

```bash
doppler run -- bash -lc 'GH_TOKEN="$GITHUB_TOKEN" <command>'
```

## Specs and docs

- `specs/` contains durable feature specifications. Read the relevant spec
  before changing behavior in that area.
- `README.md` is the public entry point; `docs/` contains standalone guidance
  for external users. Neither may link to internal `specs/` or `.agents/`
  material. See `specs/documentation.md` for the documentation contract.
- Specs capture **why** and **what**, not exhaustive **how**. Do not duplicate
  fields, enum variants, or exact tool schemas already in source.

## Local dev and tests

Yolop is a small Cargo **workspace**: the root package is the `yolop` binary,
`crates/yolop-yep/` is the extension-protocol library + server SDK (published
separately for extension authors; the host depends on it for the wire types),
`crates/tuika/` is a standalone terminal-UI toolkit (layout, overlays, focus,
components over ratatui — including the streaming `Markdown` renderer and
`CodeBlock`) with its own version, powering the experimental `--fullscreen`
renderer — see `crates/tuika/README.md`, and `crates/tuika-codeformatters/` is
tuika's tree-sitter `Highlighter` implementation (kept separate so tuika core
stays grammar-free; yolop's markdown/code rendering flows through both).
`cargo test`/`clippy` at the root cover all four. For touched code:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Quick offline smoke test (no API key needed — uses the bundled `llmsim`
provider):

```bash
cargo run -- --provider llmsim -p "hi"
```

Real provider smoke test through Doppler:

```bash
doppler run -- cargo run -- --provider openai -p "hi"
```

`RUST_LOG` is honored for the tracing layer (stderr).

Component render benchmarks live under `crates/*/benches/` (Criterion). Run a
crate's suite and, when checking a change for a regression, save a baseline
first and compare against it:

```bash
cargo bench -p tuika --bench markdown -- --save-baseline before
# ...make the change...
cargo bench -p tuika --bench markdown -- --baseline before
```

Wall-clock numbers are too noisy on shared CI runners to gate on, so CI runs the
benches only on `main` (and via manual dispatch) and uploads the Criterion
output as an artifact; regression-checking is a local, baseline-to-baseline
comparison. New component benches go in the owning crate's `benches/` as another
`[[bench]]` with `harness = false`.

Alongside those, the `*_iai` benches count CPU instructions under
Valgrind/callgrind ([iai-callgrind](https://github.com/iai-callgrind/iai-callgrind)).
Instruction counts are deterministic and machine-independent for a fixed
toolchain + libc, so the numbers are committed to `crates/*/benches/iai-baseline.json`
and the CI `iai` job **fails** on a regression past the baseline's tolerance —
this is a real gate, not an archive. Running them locally needs Valgrind and a
version-matched runner (`cargo install iai-callgrind-runner`):

```bash
rm -rf target/iai
cargo bench -p tuika --bench markdown_iai \
  -p tuika-codeformatters --bench highlight_iai -- --save-summary=json
python3 crates/tuika/benches/check_iai.py            # compare to the committed baseline
python3 crates/tuika/benches/check_iai.py --update   # bless new counts (commit alongside the change)
```

When a change legitimately shifts counts (renderer change, dependency bump,
toolchain upgrade), regenerate the baseline with `--update` and commit it with
the code — like a snapshot test. To refresh it from CI's exact environment,
run the workflow manually (`workflow_dispatch`) and commit the uploaded
`iai-baseline` artifact.

The `evals/` directory holds [Mira](https://github.com/everruns/mira) eval
studies for benchmarking yolop and other agents. `evals/swebench_verified/` is
the SWE-bench Verified study — a single self-contained `swebench_verified.py`
built on the `mira-eval` Python SDK, with PEP 723 inline deps; the `mira` host
CLI drives it from that directory (`mira --cmd "uv run swebench_verified.py"
run`). It is outside the Cargo workspace; `uv run` provisions its deps on first
use, and its tests (which import the SDK) run with `uv run --with mira-eval -m
unittest discover -s evals/swebench_verified/tests`. `evals/harness_basic/` is
a pure-Rust study on the `mira-eval` SDK (its own standalone crate, outside
this Cargo package) that A/Bs yolop harness configurations on basic coding
cases through headless `yolop -p`; `cargo test` inside that directory runs its
tests. `evals/lsp_integration/` follows the same pure-Rust pattern but isolates
the optional LSP capability on semantic-navigation traps (its `bootstrap.sh`
installs the language servers the `lsp` variant needs). See
[`evals/README.md`](evals/README.md).

## Git and commits

- Conventional Commits: `type(scope): description`.
- Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.
- Use `chore` for updates to `specs/`, `AGENTS.md`, or CI metadata.
- Never add Claude/session/AI attribution links in commits, PRs, docs, or code comments.
- Stage files explicitly by name. Avoid broad `git add .` / `git add -A`.

Commit attribution must be a real human user. If git identity is missing or
agent-like, stop and ask before committing.

## PRs and CI

- Use `.github/pull_request_template.md`. Center the description on functional
  change and impact, not a code-location walkthrough (the diff shows that). Add a
  Before / After with proof — CLI output, logs, or screenshots for UI — whenever
  behavior changes.
- PR titles must be Conventional Commits and under 70 characters.
- Use **Squash and Merge**.
- GitHub Actions is the CI source of truth.
- Never merge red CI.
- Before merge, prefer rebasing onto latest `origin/main`.

## Shipping, maintenance, and releases

- "Ship" means implement, test the changed feature with an automated test that
  exercises it, gather evidence, perform a security review, open a mergeable PR,
  address every review comment, and merge only after CI is green.
- When asked to ship, follow [`.agents/skills/ship/SKILL.md`](.agents/skills/ship/SKILL.md)
  and [`specs/shipping.md`](specs/shipping.md).
- When asked for maintenance or release readiness, follow
  [`.agents/skills/maintenance/SKILL.md`](.agents/skills/maintenance/SKILL.md)
  and [`specs/maintenance.md`](specs/maintenance.md).
- When asked to release, cut a version, or publish to crates.io / Homebrew,
  follow [`.agents/skills/release/SKILL.md`](.agents/skills/release/SKILL.md)
  and [`specs/release.md`](specs/release.md). Releases publish to both
  crates.io and the `everruns/homebrew-tap` Homebrew tap.

## Upstream relationship

Yolop is a friendly fork / promotion of the `examples/coding-cli` example in
[`everruns/everruns`](https://github.com/everruns/everruns). When the upstream
example changes meaningfully, mirror the useful parts here. Keep public
runtime crate versions in lockstep with what is published on crates.io.

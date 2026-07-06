# harness_basic — yolop harness A/B study on basic coding cases

A [Mira](https://github.com/everruns/mira) eval **study** for evaluating
**yolop feature improvements**: the same small, self-contained coding tasks run
across models × reasoning efforts × **yolop harness configurations**, so a
feature (a new tool, a capability toggle, a prompt change) can be A/B'd on pass
rate, turns, tool calls, tokens, and cost.

Unlike `swebench_verified/` (Python, benchmark-scale), this study is pure Rust
on the [`mira-eval`](https://crates.io/crates/mira-eval) SDK — no Python
plumbing — and drives yolop through its headless one-shot mode (`yolop -p`,
the runtime path; the TUI is never involved).

## The matrix

Three axes, crossed with 6 samples (~small edit / refactor / search tasks):

| Axis | Values | Where |
|------|--------|-------|
| **target** (model) | `llmsim` (offline) · `anthropic/claude-sonnet-4-5` · `anthropic/claude-opus-4-8` · `openai/gpt-5.5` · `openrouter/z-ai/glm-5.2` | `targets()` in `src/main.rs` |
| **effort** | `default` (yolop's per-model default; no flag) · `low` · `high` | `EFFORTS` |
| **harness** | `default` (out-of-the-box yolop) · `no-ast-grep` | `HARNESS_VARIANTS` |

Cloud targets gate on their key env var (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`OPENROUTER_API_KEY`) and are *skipped* when it's missing, so a keyless run
stays green on `llmsim`. Offline `llmsim` replies with a canned message, so its
cases verify plumbing only: the functional `checks` scorer reports **N/A**
there (not a failure), while `succeeded` and the budget scorers stay real.

### Harness variants — the point of this study

A variant is a yolop `settings.toml` written into an isolated per-case
`XDG_CONFIG_HOME`, on top of a base that only turns linked worktrees off
(study plumbing: the case workdir is a plain temp dir and file scorers read
edits back from it). `default` adds nothing — it is yolop out of the box.

Adding a configuration to the matrix is one entry in `src/main.rs`:

```rust
HarnessVariant {
    name: "no-repo-map",
    settings: "[[capabilities]]\nref = \"repo_map\"\nenabled = false\n",
},
```

Any `settings.toml` content works (capability toggles and configs, feature
settings), so future yolop features slot in without new plumbing. The variant
name becomes the `harness` axis value in case keys
(`basic_coding/add-fn@openai/gpt-5.5[effort=default,harness=no-ast-grep]`),
selection (`--axis harness=no-ast-grep`), and `--group-by harness`.

## How a case runs

1. Seed the sample's files into a fresh temp workdir (never a git repo).
2. Write the variant's `settings.toml` into a scratch `XDG_CONFIG_HOME`
   (config/data/state/cache are all isolated — the developer's real
   `~/.config/yolop` never leaks in).
3. Spawn `yolop -C <workdir> --provider … --model … [--reasoning-effort …]
   --session … --session-dir … -p "<prompt>"` and wait (default cap 600s,
   `HARNESS_BASIC_TIMEOUT_S`).
4. Mine the session `events.jsonl` for usage/cost (once per
   `output.message.completed`; yolop repeats usage on `reason.completed`),
   tool calls + failures, iterations/turns, the final response, and the
   reasoning effort actually applied.
5. Read the workdir back so file scorers grade what yolop actually wrote.

Raw per-case `events.jsonl` logs are kept under the gitignored
`.cache/sessions/` and each case's `metadata.session_log` records the path.

## Scoring

Each sample declares its own assertions as data (`checks` metadata), graded by
one generic scorer — adding a sample needs no new code:

```json
{"file": "src/lib.rs", "contains": ["fn greet"], "lacks": ["TODO"]}
{"response_contains": ["7321"]}
```

Alongside it: `succeeded` (yolop exited cleanly) and lenient guardrail budgets
(`turns_within(32)`, `tool_calls_within(64)`, `cost_within($2)`). The A/B
comparison itself reads the per-case numbers the transcript carries — tokens,
cost, `llm_calls`, `turns`, `tool_calls_failed`, `agent_reported_ms` — plus
metadata (`provider`, `model`, `effort`, `reasoning_effort_applied`,
`harness`, `stop_reason`) for `--group-by`.

## Setup

```bash
cargo build --release            # at the repo root: the yolop under test
cargo install mira-cli --locked  # the host CLI (or: brew install everruns/tap/mira)
```

The study binary itself needs nothing else — `mira.toml` declares a
`cargo run -q --bin harness_basic` launcher, so the first `mira` invocation
compiles it. `HARNESS_BASIC_YOLOP_BIN` overrides the yolop binary (falls back
to `target/release/yolop`, then `target/debug/yolop`).

## Usage

```bash
cd evals/harness_basic     # so mira.toml is found; runs archive to ./results

mira list                  # the eval, samples, targets, axes
mira run --preset smoke    # offline: llmsim × both harness variants, no key

# The core loop: A/B the harness variants on one model.
doppler run -- mira run --preset harness-compare --group-by harness

# Reasoning-effort sweep, out-of-the-box harness.
doppler run -- mira run --preset effort-compare --group-by effort

# Out-of-the-box yolop across the model matrix.
doppler run -- mira run --preset models --group-by target

# Ad-hoc slicing composes with any of the above:
doppler run -- mira run --targets 'anthropic/*' --axis harness=no-ast-grep --samples add-fn
```

| Preset | Purpose | Targets | Axes |
|--------|---------|---------|------|
| `smoke` | offline plumbing check, keyless & free | llmsim | effort=default |
| `harness-compare` | **A/B yolop configurations** | claude-sonnet-4-5 | effort=default, all harness |
| `effort-compare` | effort sweep | gpt-5.5 | harness=default, all efforts |
| `models` | model sweep, out-of-the-box yolop | all | harness=default, effort=default |

Every run archives to `results/<run_id>/` (`report.json`, `report.html`,
`meta.json`, per-case `cases/`); resume an interrupted run with
`mira run --resume <run_id>`. Note: yolop validates `--reasoning-effort`
against the selected model's supported values, so an unsupported
model × effort combination fails that case with yolop's error — subset the
axis rather than treating those rows as signal.

**Baseline:** the committed `results/` run is the offline smoke (12/12 pass,
llmsim, both harness variants). Verified separately: with the `no-ast-grep`
settings applied, the `ast_grep` tool is absent from the registered toolset
(and each case's input tokens drop by the removed schema's size).

## Layout

```
evals/harness_basic/
  src/main.rs   # the whole study: matrix, samples + declarative checks,
                #   yolop subject (spawn + events.jsonl mining), unit tests
  Cargo.toml    # standalone crate (outside the yolop package), mira-eval SDK
  mira.toml     # host config: launcher, ./results, presets
  results/      # mira run archives (<run_id>/)
  .cache/       # gitignored: raw per-case session logs
```

Tests: `cargo test` from this directory (event mining, settings variants,
checks scorer, matrix shape). Also `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` apply, as everywhere in the repo.

# harness_basic — yolop harness A/B study on basic coding cases

A [Mira](https://github.com/everruns/mira) eval **study** for evaluating
**yolop feature improvements**: the same small, self-contained coding tasks run
across models × reasoning efforts × **yolop harness configurations**, so a
feature (a new tool, a capability toggle, a prompt change) can be A/B'd on pass
rate, turns, tool calls, tokens, cost, and trajectory metrics such as
exploration before first mutation.

Unlike `swebench_verified/` (Python, benchmark-scale), this study is pure Rust
on the [`mira-eval`](https://crates.io/crates/mira-eval) SDK — no Python
plumbing — and drives yolop through its headless one-shot mode (`yolop -p`,
the runtime path; the TUI is never involved). The `with-ast-edit` variant
exercises the opt-in `ast_edit` capability; see [`specs/ast_edit.md`](../../specs/ast_edit.md).

## The matrix

Three axes, crossed with 12 samples (small edit / refactor / search / guardrail / structural-rewrite tasks):

| Axis | Values | Where |
|------|--------|-------|
| **target** (model) | `anthropic/claude-sonnet-4-5` · `anthropic/claude-opus-4-8` · `openai/gpt-5.5` · `openrouter/z-ai/glm-5.2` | `targets()` in `src/main.rs` |
| **effort** | `default` (yolop's per-model default; no flag) · `low` · `high` | `EFFORTS` |
| **harness** | `default` (out-of-the-box yolop) · `with-ast-edit` (opt-in `ast_edit` capability) · `no-progress-guard` · `no-ast-grep` | `HARNESS_VARIANTS` |

Every target gates on its provider key env var (`ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, `OPENROUTER_API_KEY`) and is *skipped* (not failed) when the
key is missing — a keyless run is a no-op, not a wall of red.

## Samples — what each case tests

Small, deterministic, seeded-workdir tasks; each declares its own pass
criteria (see [Scoring](#scoring)). Together they cover the harness surfaces a
feature change is most likely to move: targeted edits, multi-file
search/refactor, and read-only code navigation.

| Sample | Seed | Task | Passes when | Exercises |
|--------|------|------|-------------|-----------|
| `add-fn` [smoke] | Rust lib with empty `src/lib.rs` | Add `pub fn greet()` returning `"hello, yolop"` | `src/lib.rs` contains `fn greet` and `hello, yolop` | basic read → edit loop |
| `fix-off-by-one` | `sum()` that drops the last element via `take(len-1)` | Fix it to sum every element | `src/lib.rs` keeps `fn sum`, no longer contains `take(` | bug comprehension, minimal in-place fix |
| `rename-across-files` | Python: `fetcher.py` defines `fetch_records`; `app.py`/`report.py` import + call it | Rename to `load_records` everywhere | all 3 files contain the new name, none contains the old | project-wide search + consistent multi-file edit (where ast-grep/grep should shine) |
| `find-constant` [smoke] | 3 Python files; `MAGIC_TIMEOUT_MS = 7321` buried in `settings/defaults.py` | Answer its value, number only | final response contains `7321` | read-only navigation/search, no edits |
| `implement-todo` | JS `clamp()` stub that throws, with a TODO spec comment | Implement per the TODO, remove the comment | `utils.js` has `function clamp` + `module.exports`, no `TODO`/`not implemented` | spec-comment comprehension, stub completion |
| `add-module` | Rust lib with one existing fn | Create `src/util.rs` with `pub fn double`, wire `pub mod util;` into lib.rs | both files contain the required items | new-file creation + wiring across files |
| `progress-guard-sequential-read` [`progress-guard`] | 24 numbered notes; answer in `notes/24.txt` | Read notes sequentially and answer the final code | final response contains `KITE-7429` | long exploration streak that should trigger `progress_guard` |
| `progress-guard-checkpoint-read` [`progress-guard`] | 50 numbered notes; answer in `checkpoint/50.txt` | Read notes sequentially and answer the final code | final response contains `WREN-5081` | escalation from first warning to checkpoint warnings |
| `background-callback-bridge` [`progress-guard`] | Rust crate where `spawn_background` completions land in `SessionTaskRegistry`, but app wake only drains legacy background state | Fix the callback bridge while keeping the legacy wake test passing | source drains session-task completions and keeps focused regression tests | realistic investigation based on the background-callback failure mode |
| `replace-console-log` [`ast-edit`] | TS: `api.ts`/`worker.ts` call `console.log`; `logger.ts` exports `logger.info` | Replace every `console.log(...)` with `logger.info(...)` | both TS files use `logger.info`, no `console.log` | multi-file shape rewrite (`console.log` → `logger.info`) |
| `strip-print-debug` [`ast-edit`] | Python `app.py`/`helpers.py` with standalone `print(...)` debug lines | Remove every standalone `print(...)` statement | neither file contains `print(` | bulk statement removal across files |
| `unwrap-to-expect` [`ast-edit`] [smoke] | Rust `src/lib.rs` with `.unwrap()` on `first`/`last` | Replace every `.unwrap()` with `.expect("failed")` | file contains `.expect("failed")`, no `.unwrap()` | small Rust structural rewrite |

### Harness variants — the point of this study

A variant is a yolop `settings.toml` written into isolated per-case config dirs
(`XDG_CONFIG_HOME` and a scratch `HOME` for macOS), on top of a base that only turns linked worktrees off
(study plumbing: the case workdir is a plain temp dir and file scorers read
edits back from it). `default` adds nothing — it is yolop out of the box.
`with-ast-edit` enables the opt-in `ast_edit` capability for previewed
structural rewrites. `no-progress-guard` disables the progress-guard capability
so guardrail changes can be compared against the same binary with that
capability removed.

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
(`basic_coding/add-fn@openai/gpt-5.5[effort=default,harness=no-progress-guard]`),
selection (`--axis harness=no-progress-guard`), and `--group-by harness`.

## How a case runs

1. Seed the sample's files into a fresh temp workdir (never a git repo).
2. Write the variant's `settings.toml` into scratch config dirs
   (config/data/state/cache are all isolated — the developer's real
   `~/.config/yolop` never leaks in).
3. Spawn `yolop -C <workdir> --provider … --model … [--reasoning-effort …]
   --session … --session-dir … -p "<prompt>"` and wait (default cap 600s,
   `HARNESS_BASIC_TIMEOUT_S`).
4. Mine the session `events.jsonl` for usage/cost (once per
   `output.message.completed`; yolop repeats usage on `reason.completed`),
  tool calls + failures, trajectory counters, iterations/turns, the final
  response, and the reasoning effort actually applied.
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
cost, `llm_calls`, `turns`, `tool_calls_failed`, `agent_reported_ms`,
`exploration_tools_before_first_mutation`, `max_exploration_tools_without_progress`,
`progress_guard_warnings`, `ast_edit_tool_calls`, and `ast_edit_tool_calls_failed`
— plus metadata (`provider`, `model`, `effort`,
`reasoning_effort_applied`, `harness`, `stop_reason`) for `--group-by`.

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

# Cheap sanity check: 2 smoke-tagged samples × both harness variants, one model.
doppler run -- mira run --preset smoke

# The core loop: A/B the harness variants on one model.
doppler run -- mira run --preset harness-compare --group-by harness

# Focused guardrail proof: default vs no-progress-guard on the long-read trap.
doppler run -- mira run --preset progress-guard --group-by harness

# Structural-rewrite A/B: default vs with-ast-edit on ast-edit-tagged cases.
doppler run -- mira run --preset ast-edit-compare --group-by harness

# Reasoning-effort sweep, out-of-the-box harness.
doppler run -- mira run --preset effort-compare --group-by effort

# Out-of-the-box yolop across the model matrix.
doppler run -- mira run --preset models --group-by target

# Ad-hoc slicing composes with any of the above:
doppler run -- mira run --targets 'anthropic/*' --axis harness=no-ast-grep --samples add-fn
```

| Preset | Purpose | Samples | Targets | Axes |
|--------|---------|---------|---------|------|
| `smoke` | cheap sanity check (4 cases) | tag `smoke` | claude-sonnet-4-5 | effort=default, all harness |
| `harness-compare` | **A/B yolop configurations** | all | claude-sonnet-4-5 | effort=default, all harness |
| `progress-guard` | focused warning-behavior check | tag `progress-guard` | claude-sonnet-4-5 | effort=default, default vs no-progress-guard |
| `ast-edit-compare` | **A/B ast_edit capability** | tag `ast-edit` | claude-sonnet-4-5 | effort=default, default vs with-ast-edit |
| `effort-compare` | effort sweep | all | gpt-5.5 | harness=default, all efforts |
| `models` | model sweep, out-of-the-box yolop | all | all | harness=default, effort=default |

Every run archives to `results/<run_id>/` (`report.json`, `report.html`,
`meta.json`, per-case `cases/`); resume an interrupted run with
`mira run --resume <run_id>`. Note: yolop validates `--reasoning-effort`
against the selected model's supported values, so an unsupported
model × effort combination fails that case with yolop's error — subset the
axis rather than treating those rows as signal.

Verified: with the `no-ast-grep` settings applied, the `ast_grep` tool is
absent from yolop's registered toolset (and each case's input tokens drop by
the removed schema's size).

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

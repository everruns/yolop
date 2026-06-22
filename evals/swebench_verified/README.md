# swebench_verified — yolop benchmark study

Benchmarks yolop on coding benchmarks, starting with **SWE-bench Verified**.
swebench_verified is a [Mira](https://github.com/everruns/mira) eval **study**: the
generic `mira` host CLI owns the target matrix, selection, checkpoints, and
JSON/HTML/JUnit reporting, while this Python study — driven over Mira's stdio
protocol — owns the SWE-bench-specific work (loading instances, checking out
repos, running agent CLIs, and the Docker `FAIL_TO_PASS` scoring). Designed to
grow: new benchmarks and new agents plug in behind small interfaces.

See [Usage](#usage) for the `mira --cmd …` invocations.

## Agents

The harness benchmarks yolop and, behind the same `Agent` interface, three other
terminal coding agents so they can be compared on identical instances, prompt,
and scorer:

| `agent:` | CLI | needs | cost source |
|----------|-----|-------|-------------|
| `yolop` | `target/release/yolop` | provider key | yolop's own usage accounting |
| `claude-code` | `claude` on PATH | `ANTHROPIC_API_KEY` | reported `total_cost_usd` |
| `codex` | `codex` on PATH | `OPENAI_API_KEY` | estimated from config `price` |
| `pi` | `pi` on PATH | provider key | reported `usage.cost.total` |

Each adapter runs its CLI non-interactively in the checkout with edits
auto-approved, streams the tool's JSON event log to a file, and mines it into the
common `RunMetrics`. The study captures the working-tree diff regardless of
agent, so scoring is identical.

### First comparison

Seven configs on `astropy__astropy-12907` (SWE-bench Verified), same prompt and
Docker scorer — all **resolved**. `effort` is the reasoning effort *actually
applied* (yolop records it per request; it picks a per-model default when none is
configured):

| agent (model) | effort | resolved | cost | wall | turns | llm calls | tool calls |
|---|---|---|---|---|---|---|---|
| yolop (claude-sonnet-4-5) | high | ✓ | $0.288† | 131s | 26 | 26 | 26 |
| yolop (claude-opus-4-8) | high | ✓ | $0.114† | 122s | 10 | 10 | 9 |
| yolop (gpt-5.5, OpenAI) | medium | ✓ | $1.698† | 156s | 23 | 23 | 22 |
| yolop · OpenRouter (nvidia nemotron-3-ultra-550b) | — | ✓ | $0.718 | 607s | 71 | 71 | 70 |
| claude-code (claude-sonnet-4-5) | n/a | ✓ | $0.830 | 226s | 34 | 54 | 33 |
| codex (gpt-5.5) | n/a | ✓ | ~$0.063\* | 27s | 1 | 6 | 11 |
| pi (gpt-5.5) | n/a | ✓ | $0.099 | 22s | 7 | 14 | 6 |

\* codex reports no cost; harness estimate from a placeholder `price` block —
update to real gpt-5.5 pricing. `n/a` = the agent's CLI doesn't expose effort.

† yolop's cost for providers that don't return an inline price (OpenAI,
Anthropic) is a price-table **estimate that does not discount cached input**
(`estimate_cost_usd` bills full `prompt_tokens`). For OpenAI `prompt_tokens`
*includes* cached tokens, so cache-heavy runs are **over-stated**: the gpt-5.5
row reads $1.698 but ≈87% of its prompt was cache reads — applying the standard
cache discount gives ≈ **$0.38**, matching the same model's real OpenRouter cost
($0.36). OpenRouter and pi costs are tool-reported (real). `effort` is the model
reasoning-effort setting (`reasoning_effort`; per-model default when unset — see the
`openai-high` config for `high`). One instance is a smoke, not a resolve-rate;
run the full set per config for leaderboard-comparable numbers.

## What it measures

For every `(instance, config)` cell it records, mined from the agent's event log
and surfaced in the host's transcript/usage:

- **success** — `resolved` (did the hidden test suite pass?)
- **time** — `wall_time_s` (harness-measured) and `agent_reported_time_s` (from the agent)
- **turns**/**iterations**, **assistant/user messages**, **llm_calls**
- **tool calls** — total, failed, and a per-tool breakdown (`tools_used`)
- **tokens** — input, output, `cache_read_tokens`, `cache_creation_tokens`, total
- **cost** — `cost_usd` (tool-reported where available, else estimated from `price`)
- **efficiency** — cost per resolved instance ("score per dollar"), computable
  from the host's per-cell `resolved` + `cost_usd`
- **stop_reason** — `completed` / `timeout` / `budget` / `error`
- **config metadata** — agent, provider, model, reasoning effort, stop reason

Each cell's transcript carries these on Mira's open channels: a numeric
**`metrics`** map (`turns`, `iterations`, `tool_calls`, `tool_calls_failed`,
cache tokens, `agent_reported_time_s`) that feeds the host's generic budget
scorers, and structured **`metadata`** (`agent`, `provider`, `model`,
`reasoning_effort`, `stop_reason`, `resolved`, `repo`, `difficulty`,
`tools_used`, and the full SWE-bench `eval_report` — `FAIL_TO_PASS`/`PASS_TO_PASS`
status). Tokens/cost/latency stay on the typed `usage`/`timing` fields. A
Docker/harness failure to score is reported as an **infra error** (`error_kind`),
so the host treats it as **N/A** and retries it rather than counting it as the
model getting the fix wrong.

Each **sample** is tagged with `repo`/`difficulty` metadata and each **target**
column carries its config (`agent`, `model`, `reasoning_effort`, `price`, …), so
the host can break results down by any of them:

```bash
mira --cmd "$STUDY" run --tag tracking-v1 --group-by repo         # resolve rate per repo
mira --cmd "$STUDY" run --group-by difficulty                     # per difficulty tier
mira --cmd "$STUDY" run --group-by agent                          # yolop vs claude-code vs …
```

Cost is cache-aware (`cache_read`/`cache_creation` tokens are priced), so cost
per resolved instance is a fair cross-model comparison. The host surfaces all of
the above per cell in its JSON and HTML reports — emit one with
`mira ... run --format html --out report.html` (a single self-contained file) or
`--format json --out run.json` for the raw rows. Each `--save` run also stamps
`meta.json` with an `environment` block (git commit/branch/dirty, box, mira
version) plus the `benchmark` label from `mira.toml`.

## Cost cap

Each run has a per-instance USD budget (default **$5**, `max_cost_usd` in the
matrix). None of these CLIs has a native dollar cap, so the study watches the
running cost in the session log and kills the run if
it exceeds the cap (recorded as `stop_reason: budget`). Cost comes from the
tool's own reporting where available; for agents that report only tokens (codex)
set a per-config `price` block (USD per 1M tokens) so cost — and the cap — can be
computed. Without a price/reported cost, only the wall-clock `timeout` bounds a
run.

## Tracking suite

`suites/tracking-v1.json` is a fixed, representative 20-instance subset of
SWE-bench Verified for tracking yolop quality across versions. It is selected
deterministically (no randomness) by `suites/select_tracking.py`: repo quotas by
largest-remainder apportionment proportional to each repo's share of the 500
instances, then filled to match the benchmark's overall difficulty mix. The
result spans 9 repos and 3 difficulty tiers (8 `<15 min`, 10 `15 min–1 hour`,
2 `1–4 hours`), so the resolve-rate carries signal.

Every sample is tagged with the suites it belongs to, so run one with the host's
`--tag` selector:

```bash
mira --cmd "uv run swebench_verified.py" run --tag tracking-v1 --targets openai-default --save
```

Re-running `select_tracking.py` on the same dataset reproduces the committed set
exactly; bump to `tracking-v2` rather than editing v1, so historical numbers stay
comparable.

**Baseline:** yolop · gpt-5.5 (OpenAI) scores **14/20** on tracking-v1 (all cases
ran to completion). Point `--checkpoint` at a per-suite path (e.g.
`--checkpoint ck/tracking-v1.json`) to keep a run resumable and its results
self-contained.

## Layout

```
evals/swebench_verified/
  swebench_verified.py   # the whole study: one file, PEP 723 inline deps
                         #   matrix, agents (yolop/claude-code/codex/pi), event-log
                         #   metrics, SWE-bench load + Docker scoring, protocol loop
  mira.toml              # host config: [results].dir -> ./results
  suites/                # curated instance subsets (tracking-v1.json + selector)
  tests/                 # stdlib-only unit tests (run with plain python3)
  results/               # mira --save run archives (<run_id>/) + pre-Mira history
  .cache/                # gitignored: dataset parquet, checkouts, raw logs, eval scratch
```

The study is a single self-contained file with its dependencies declared inline
([PEP 723](https://peps.python.org/pep-0723/)), so `uv run swebench_verified.py`
builds an ephemeral env and runs it — no package, `pyproject.toml`, or venv.

The `mira` host owns durable run output. `--save` archives a self-contained run
folder under `results/` (`results/<run_id>/{report.json,report.html,meta.json}`,
`run_id = YYYYMMDDThhmmssZ-xxxx`); the dir comes from `mira.toml`'s
`[results].dir`. For a resumable long run point `--checkpoint <path>` at a JSON
session (saved after every cell); for a one-off report use `--format html --out
report.html`. Raw `events.jsonl` logs stay in `.cache/sessions/` and are **not**
committed (see [Session data upload](#session-data-upload)). The `swebench_verified/`
subdirs under `results/` are pre-Mira historical runs in the old per-config format.

## Setup

```bash
evals/swebench_verified/bootstrap.sh    # pre-warm uv deps + yolop build + mira + agent CLIs
```

`bootstrap.sh` is idempotent: it pre-warms the `uv` dependency cache, builds
yolop release, installs the `mira` host CLI, and installs the three external
agent CLIs (claude-code, codex, pi) pinned to the validated versions. Scope it
with `AGENTS=codex,pi evals/swebench_verified/bootstrap.sh`. Manual equivalent:

```bash
( cd ../.. && cargo build --release )    # produces target/release/yolop
brew install everruns/tap/mira           # the host CLI that drives the study
# Python deps need nothing — `uv run swebench_verified.py` installs them on first use.
```

Requires a running **Docker** daemon (SWE-bench runs the hidden tests in
per-instance containers) and provider keys in the environment: `OPENAI_API_KEY`
(yolop openai / codex / pi), `ANTHROPIC_API_KEY` (yolop anthropic / claude-code),
`OPENROUTER_API_KEY` (yolop openrouter configs). Use `doppler run --` to inject
secrets. **codex** ignores `OPENAI_API_KEY` for requests — log in once with
`printenv OPENAI_API_KEY | codex login --with-api-key`. To benchmark a non-yolop
agent, its CLI must be installed and on `PATH` (`claude`, `codex`, or `pi`).

Pin those agent CLIs to the versions recorded in `bootstrap.sh` so a saved run is
reproducible against the binaries it was produced with.

## Usage

`swebench_verified` is a [Mira](https://github.com/everruns/mira) eval study: the `mira`
host CLI drives it over a stdio JSON protocol, owning the matrix, selection,
checkpoints, and reporting, while the study owns the SWE-bench-specific run +
Docker scoring. Drive it with `--cmd` pointed at the script:

```bash
cd evals/swebench_verified      # so the adjacent mira.toml is found; --save lands in ./results
STUDY="uv run swebench_verified.py"

# What the study advertises: the eval, its samples (instances), and the models
# (matrix configs); configs with a missing provider key show as unavailable.
mira --cmd "$STUDY" list

# Plumbing only: one instance on the offline llmsim config, skip Docker eval.
SWEBENCH_NO_EVAL=1 mira --cmd "$STUDY" run astropy__astropy-12907 --targets llmsim

# Real end-to-end on one instance, selected configs (substring selection on the
# case key, like `cargo test`), archived under ./results.
doppler run -- mira --cmd "$STUDY" run astropy__astropy-12907 \
    --targets openai-default,openai-high --save

# Whole benchmark (all 500), one config, resumable + saved run archive.
doppler run -- mira --cmd "$STUDY" run --targets openai-default \
    --checkpoint ck/openai-default.json --save
```

Study-internal knobs that the host doesn't own are read from the environment so
they can be set on the `--cmd` line: `SWEBENCH_NO_EVAL=1` (skip Docker scoring),
`SWEBENCH_MAX_WORKERS=N` (Docker eval parallelism), `SWEBENCH_NAMESPACE=none`
(build images locally instead of pulling prebuilt), `SWEBENCH_EVAL_TIMEOUT`,
`SWEBENCH_YOLOP_BIN` (override the yolop binary). The per-instance USD cap is set
per config in the matrix (`max_cost_usd`, default `$5`).

### Recipe: cross-agent comparison

The comparison target set is the named **`compare` preset** in `mira.toml`
(`[presets.compare]`), applied with `mira run --preset compare`. The
[`compare.sh`](compare.sh) wrapper pairs it with the instance, `--group-by
agent`, and `--save`:

```bash
./compare.sh astropy__astropy-12907            # the compare preset, --group-by agent --save
./compare.sh django__django-11099 -j 4         # any instance, extra mira flags pass through
COMPARE_TARGETS=codex,pi,claude-code ./compare.sh <id>   # override the set ad-hoc
```

It runs one cell per agent flavor (yolop across providers/effort + claude-code,
codex, pi, with Opus 4.8 on yolop and claude-code), groups by `agent`, and saves
the run under `results/<run_id>/`. A **target** is Mira's comparison axis — a
model *or* a harness — so agent configs are first-class targets, not models
faked into the model slot.

A preset can also **pin the sample** via `filter` (a substring on the case key
`eval/sample@target`), so "this one item across all targets" is a single
self-contained preset — no instance arg:

```bash
# [presets.astropy-12907] = filter = "astropy__astropy-12907" + the compare targets
mira --cmd "uv run swebench_verified.py" run --preset astropy-12907 --group-by agent --save
```

Clone that `[presets.NAME]` block (new name + `filter`) to pin other instances.

## Config matrix

The matrix is the `MATRIX` dict near the top of `swebench_verified.py` — one
entry per config, each a Mira target (label = config name). `agent:` picks the
adapter (`yolop` | `claude-code` | `codex` | `pi`); remaining keys go to it.
`DEFAULTS` (timeout, `max_cost_usd`) applies to all. Add a config by appending an
entry:

```python
MATRIX = {
    "anthropic-sonnet": {"agent": "yolop", "provider": "anthropic", "model": "claude-sonnet-4-5"},
    "claude-code":      {"agent": "claude-code", "model": "claude-sonnet-4-5"},
    "codex":            {"agent": "codex", "model": "gpt-5.5",
                         "price": {"input": 1.25, "output": 10.0, "cache_read": 0.125}},
    "pi":               {"agent": "pi", "provider": "openai", "model": "gpt-5.5"},
}
```

yolop configs run the binary at `target/release/yolop` (override with
`SWEBENCH_YOLOP_BIN`); the other agents run their CLI from `PATH`.

## Extending

It's one file — extend it in place:

- **New agent:** add a `run_<agent>(cfg, instance, workdir, session_dir) -> AgentRun`
  (use the shared `run_agent_process` driver + an event-log `extract_*` parser),
  register it in the `_AGENTS` dispatch, and reference it as `agent: <name>` in
  `MATRIX`.
- **New benchmark:** this study is SWE-bench-specific; a different benchmark is a
  sibling study folder under `evals/` with its own single-file adapter.

## Session data upload

Full `events.jsonl` logs are large and noisy, so they stay out of git (in
`.cache/sessions/`). The agent records each log's path; the cell transcript
returned to the host carries the metrics mined from it. Uploading these logs to
durable storage (object store /
dataset) is a deliberate, still-open integration point — the harness keeps the
logs intact and addressable so an uploader can be bolted on without changing the
run path. **TODO:** wire up an uploader (and record the resulting URL on each
result record).

## How an instance runs

The host asks the study to run one `(instance, config)` cell at a time; for each:

1. Shallow-fetch the repo at `base_commit` into a scratch worktree.
2. Run `yolop -C <worktree> -p "<problem statement prompt>" --session-dir …`.
3. Capture `git diff` as the model patch.
4. Hand the patch to SWE-bench's official Docker evaluator and parse the
   `report.json` for `resolved`, returned to the host as the cell's score.
```

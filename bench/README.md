# yoloeval — yolop benchmark harness

Benchmarks yolop on coding benchmarks, starting with **SWE-bench Verified**, and
keeps the results in this repo. Designed to grow: new benchmarks and new agents
plug in behind small interfaces.

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
common `RunMetrics`. The runner captures the working-tree diff regardless of
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

For every `(instance, config)` run it records, mined from the agent's event log:

- **success** — `resolved` (did the hidden test suite pass?)
- **time** — `wall_time_s` (harness-measured) and `agent_reported_time_s` (from the agent)
- **turns**/**iterations**, **assistant/user messages**, **llm_calls**
- **tool calls** — total, failed, and a per-tool breakdown (`tools_used`)
- **tokens** — input, output, `cache_read_tokens`, `cache_creation_tokens`, total
- **cost** — `cost_usd` (tool-reported where available, else estimated from `price`)
- **stop_reason** — `completed` / `timeout` / `budget` / `error`
- **config metadata** — agent, provider, model, reasoning effort, cost cap, tool version

## Cost cap

Each run has a per-instance USD budget (default **$5**, `max_cost_usd` in the
matrix or `--max-cost` on the CLI). None of these CLIs has a native dollar cap,
so the harness watches the running cost in the session log and kills the run if
it exceeds the cap (recorded as `stop_reason: budget`). Cost comes from the
tool's own reporting where available; for agents that report only tokens (codex)
set a per-config `price` block (USD per 1M tokens) so cost — and the cap — can be
computed. Without a price/reported cost, only the wall-clock `timeout` bounds a
run.

## Layout

```
bench/
  yoloeval/            # the harness (Python package)
    datasets/          # Benchmark implementations (swebench.py)
    agents/            # Agent adapters (yolop, claude_code, codex, pi; _proc shared driver)
    metrics.py         # yolop events.jsonl -> RunMetrics
    agent_metrics.py   # claude-code / codex / pi event logs -> RunMetrics
    pricing.py         # token-based cost estimate (fallback for codex)
    runner.py          # (instance x config) orchestration
    results.py         # save/summarize result JSON
  configs/matrix.yaml  # the config matrix to benchmark
  results/             # committed: per-run JSON + summary.json per config
  .cache/              # gitignored: dataset parquet, checkouts, raw logs, eval scratch
```

Results path: `results/<benchmark>/<config>/<instance_id>.json` plus a rolled-up
`summary.json`. Per-run JSON holds config + metrics + the model patch +
`resolved`. Raw `events.jsonl` logs are **not** committed (see
[Session data upload](#session-data-upload)).

## Setup

```bash
bench/bootstrap.sh          # venv + yolop build + agent CLIs (pinned versions)
```

`bootstrap.sh` is idempotent and sets up the Python venv, a yolop release build,
and the three external agent CLIs (claude-code, codex, pi) pinned to the
validated versions. Scope it with `AGENTS=codex,pi bench/bootstrap.sh`. Manual
equivalent:

```bash
python3 -m venv bench/.venv && bench/.venv/bin/pip install -r bench/requirements.txt
cargo build --release       # produces target/release/yolop
```

Requires a running **Docker** daemon (SWE-bench runs the hidden tests in
per-instance containers) and provider keys in the environment: `OPENAI_API_KEY`
(yolop openai / codex / pi), `ANTHROPIC_API_KEY` (yolop anthropic / claude-code),
`OPENROUTER_API_KEY` (yolop openrouter configs). Use `doppler run --` to inject
secrets. **codex** ignores `OPENAI_API_KEY` for requests — log in once with
`printenv OPENAI_API_KEY | codex login --with-api-key`. To benchmark a non-yolop
agent, its CLI must be installed and on `PATH` (`claude`, `codex`, or `pi`).

Every result records the exact tool version that produced it (e.g.
`codex_version`, `pi_version`, `claude_code_version`, `yolop_version`) in its
`agent` block, so a committed result is always traceable to its binary.

## Usage

```bash
cd bench

# Plumbing only: first instance, default config, skip Docker eval
.venv/bin/python -m yoloeval run --config llmsim --limit 1 --no-eval

# Real end-to-end on the first instance
.venv/bin/python -m yoloeval run --config openai-default --limit 1

# A specific instance across selected configs
.venv/bin/python -m yoloeval run --instance astropy__astropy-12907 \
    --config openai-default --config openai-high

# Whole benchmark (all 500), one config
.venv/bin/python -m yoloeval run --config openai-default

# Re-score saved patches without re-running the agent (e.g. after a Docker Hub
# pull rate-limit aborted the original eval)
.venv/bin/python -m yoloeval eval --instance astropy__astropy-12907 --config openai-default

# Rebuild summaries from saved results
.venv/bin/python -m yoloeval summarize --config openai-default
```

Useful flags: `--max-cost 5` (per-instance USD cap), `--max-workers N` (Docker
eval parallelism), `--namespace none` (build images locally instead of pulling
prebuilt), `--eval-timeout`, `--keep-workdirs`.

## Config matrix

`configs/matrix.yaml` declares the configurations to benchmark. `defaults`
applies to all; per-config keys override. Each config is one results column.

```yaml
defaults:
  timeout: 1800
  max_cost_usd: 5.0
configs:
  anthropic-sonnet: { agent: yolop, binary: ../../target/release/yolop,
                      provider: anthropic, model: claude-sonnet-4-5 }
  claude-code:      { agent: claude-code, model: claude-sonnet-4-5 }
  codex:            { agent: codex, model: gpt-5-codex,
                      price: { input: 1.25, output: 10.0, cache_read: 0.125 } }
  pi-sonnet:        { agent: pi, provider: anthropic, model: claude-sonnet-4-5 }
```

## Extending

- **New benchmark:** add a `Benchmark` subclass in `yoloeval/datasets/` (implement
  `load` + `evaluate`) and register it in `datasets/base.get_benchmark`.
- **New agent (to compare against yolop):** add an `Agent` subclass in
  `yoloeval/agents/` (implement `run` to leave changes in the working tree, using
  the shared `_proc.run_agent_process` driver) and register it in
  `agents._AGENTS`. Set `agent: <name>` in a matrix entry.

## Session data upload

Full `events.jsonl` logs are large and noisy, so they stay out of git (in
`.cache/sessions/`). The per-run result JSON references each log by
`session_log_path`. Uploading these logs to durable storage (object store /
dataset) is a deliberate, still-open integration point — the harness keeps the
logs intact and addressable so an uploader can be bolted on without changing the
run path. **TODO:** wire up an uploader (and record the resulting URL on each
result record).

## How an instance runs

1. Shallow-fetch the repo at `base_commit` into a scratch worktree.
2. Run `yolop -C <worktree> -p "<problem statement prompt>" --session-dir …`.
3. Capture `git diff` as the model patch.
4. After all instances for a config, hand the patches to SWE-bench's official
   Docker evaluator and parse the per-instance `report.json` for `resolved`.
```

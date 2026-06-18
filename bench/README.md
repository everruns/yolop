# yoloeval — yolop benchmark harness

Benchmarks yolop on coding benchmarks, starting with **SWE-bench Verified**, and
keeps the results in this repo. Designed to grow: new benchmarks and new agents
plug in behind small interfaces.

## What it measures

For every `(instance, config)` run it records, from yolop's `events.jsonl`:

- **success** — `resolved` (did the hidden test suite pass?)
- **time** — `wall_time_s` (harness-measured) and `agent_reported_time_s` (summed turn durations)
- **turns**, **assistant/user messages**, **llm_calls**
- **tool calls** — total, failed, and a per-tool breakdown (`tools_used`)
- **tokens** — input, output, `cache_read_tokens`, `cache_creation_tokens`, total
- **cost** — `cost_usd` (provider `actual_cost_usd` when present, else yolop's `estimated_cost_usd`)
- **stop_reason** — `completed` / `timeout` / `budget` / `error`
- **config metadata** — agent, provider, model, reasoning effort, cost cap, yolop version

## Cost cap

Each run has a per-instance USD budget (default **$5**, `max_cost_usd` in the
matrix or `--max-cost` on the CLI). yolop has no native cost cap, so the harness
watches the running cost in the session log and kills the run if it exceeds the
cap (recorded as `stop_reason: budget`). Cost is read from yolop's own usage
accounting, so it tracks whatever pricing yolop knows for the model.

## Layout

```
bench/
  yoloeval/            # the harness (Python package)
    datasets/          # Benchmark implementations (swebench.py)
    agents/            # Agent adapters (yolop.py; add others here)
    metrics.py         # events.jsonl -> RunMetrics
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
python3 -m venv bench/.venv
bench/.venv/bin/pip install -r bench/requirements.txt
cargo build --release            # produces target/release/yolop
```

Requires a running **Docker** daemon (SWE-bench runs the hidden tests in
per-instance containers) and a provider key in the environment
(`OPENAI_API_KEY` by default; `ANTHROPIC_API_KEY` for the Anthropic config).
Use `doppler run --` to inject secrets where applicable.

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
  agent: yolop
  binary: ../../target/release/yolop
  timeout: 1800
configs:
  openai-default: { provider: openai, model: gpt-5.5 }
  openai-high:    { provider: openai, model: gpt-5.5, reasoning_effort: high }
```

## Extending

- **New benchmark:** add a `Benchmark` subclass in `yoloeval/datasets/` (implement
  `load` + `evaluate`) and register it in `datasets/base.get_benchmark`.
- **New agent (to compare against yolop):** add an `Agent` subclass in
  `yoloeval/agents/` (implement `run` to leave changes in the working tree) and
  register it in `agents.build_agent`. Set `agent: <name>` in a matrix entry.

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

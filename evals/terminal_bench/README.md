# terminal_bench, yolop on Terminal-Bench 2.1

Benchmarks yolop on [**Terminal-Bench 2.1**](https://www.tbench.ai/news/terminal-bench-2-1):
89 containerized terminal tasks, compile a toolchain, recover a corrupted
database, break a cipher, each scored by the task's own test suite. 2.1 is a
revision of 2.0 that fixes 28 tasks, so **2.0 and 2.1 numbers are not
comparable**; cite the version with any result.

Two harnesses stack, each owning what it is good at:

| Layer | Owns |
|-------|------|
| [Mira](https://github.com/everruns/mira) (`mira` CLI) | the target matrix, sample/target selection, saved run folders, reporting, the same role it plays for [`swebench_verified`](../swebench_verified/) |
| [Harbor](https://harborframework.com) (`harbor` CLI) | one case: build the task container, run the agent in it, run the task's verifier |

`terminal_bench.py` is the Mira study; it shells out to `harbor run` per case and
reads the reward back out of the trial's `result.json`. `harbor_yolop.py` is the
Harbor **agent adapter** that puts yolop inside the container.

## How yolop plugs into Harbor

Harbor selects an agent with `--agent <module>:<Class>`, so the adapter is loaded
by import path rather than being registered in Harbor's own agent map. yolop is a
single static binary, not an npm/pip package, so `install()` **uploads a
host-built binary** into the container instead of fetching a release.

Two yolop features do the heavy lifting:

- **`--trajectory-out` writes ATIF v1.7**: the same format Harbor consumes
  (see [`knowledge/specs/trajectory.md`](../../knowledge/specs/trajectory.md)).
  The adapter points it at `/logs/agent/trajectory.json`, declares
  `SUPPORTS_ATIF`, and hands the document over unconverted. Token totals and
  cost come from its `final_metrics`.
- **`--session-dir` writes `events.jsonl` incrementally**, and `/logs/agent` is
  bind-mounted from the host trial dir, so the host can watch cost as it
  accrues and stop the run at `max_cost_usd`. yolop has no native dollar cap.

The adapter also applies `prompt_autonomous.md` to every instruction by default
(override with `--ak prompt_template_path=<path>`). Terminal-Bench instructions
are written as if to a colleague, "please help me find them and merge them into
master", but there is nobody in the container to answer a follow-up, and grading
looks only at the filesystem after the agent exits, so a run that stops to ask
for confirmation scores zero with the work undone. The template says so.

`worktrees = "off"` is written into an isolated `XDG_CONFIG_HOME` at install:
Terminal-Bench containers are not git repos and the verifier reads the live
filesystem, so edits must land in place rather than on a linked branch.

### ABI: the binary must match the image

Harbor **always** runs the agent inside the task's container, and Terminal-Bench
images are mostly Debian bookworm (glibc 2.36), older than a typical dev box, so
a host glibc build will not start there. Build the static musl binary
(`bootstrap.sh` does this):

```bash
cargo build --release --target x86_64-unknown-linux-musl
```

Point `TB_YOLOP_BIN` elsewhere to override it.

## First run

Two `easy` tasks, yolop · gpt-5.6-terra (via OpenRouter, so `cost_usd` is the
real charge), run `20260730T205010Z-4dc1`, **2/2 resolved for $0.20**:

| task | resolved | cost | wall | llm calls | tool calls |
|---|---|---|---|---|---|
| `fix-git` | ✓ | $0.091 | 73s | 13 | 16 |
| `cobol-modernization` | ✓ | $0.106 | 85s | 16 | 20 |

Two cents of signal, not a resolve rate, run the full 89 for a headline number.
Both cases needed a fix that the run surfaced, and both are worth knowing about
before reading any Terminal-Bench result:

- **The instructions have no reader.** `fix-git` asks "please help me find them
  and merge them into master"; yolop found the dangling commit, then stopped to
  ask whether it should rewrite `master`. Correct behavior with a user present,
  zero reward with none. `prompt_autonomous.md` (applied by default, see
  [How yolop plugs into Harbor](#how-yolop-plugs-into-harbor)) states that
  nothing reads the agent's output and that only the filesystem is graded.
- **The verifier needs its own network trust.** Many tasks' `test.sh` fetches
  `uv` before running the tests. The verifier phase inherits none of the agent's
  environment, so behind a TLS-terminating proxy every task fails to *score*,
  reported as a 0.0 reward that looks exactly like a wrong answer. `TB_CA_CERT`
  now also configures the verifier (see [Environment knobs](#environment-knobs)).

## What it measures

Per `(task, config)` case, surfaced in the host's transcript/usage:

- **success**: `resolved` (reward 1.0 = the task's tests all passed; Terminal-Bench is pass/fail per task)
- **time**: `wall_time_s` (harness-measured, includes container build + verify)
- **tokens**: input, output, `cache_read_tokens`; **cost**: `cost_usd` from yolop's own accounting
- **loop shape**: `agent_steps`, `llm_calls`, `tool_calls`, per-tool `tools_used`
- **stop_reason**: `completed` / `budget` / `timeout` / `error`
- **config metadata**: agent, provider, model, reasoning effort, yolop version

Samples carry `difficulty`, `category`, and the task's own keyword tags, so the
host can break results down by any of them:

```bash
mira run --group-by difficulty       # resolve rate per difficulty tier
mira run --group-by agent            # yolop vs terminus-2 vs claude-code vs codex
mira run --tag easy                  # just the easy tier
```

A Harbor/Docker failure, a harness timeout, and a budget stop are all reported as
**infra errors** (`error_kind`), so the host treats them as **N/A** and retries
rather than counting them as the model failing the task.

## Control suite

`suites/control-v1.json` is the **periodic regression set**: 8 fixed tasks
(3 easy / 3 medium / 2 hard) run on a cadence to notice yolop moving. The whole
89 is for headline numbers; this is for noticing movement, and it is cheap and
fast enough to repeat.

| task | difficulty | category | agent timeout |
|---|---|---|---|
| `overfull-hbox` | easy | debugging | 750s |
| `cobol-modernization` | easy | software-engineering | 900s |
| `fix-git` | easy | software-engineering | 900s |
| `modernize-scientific-stack` | medium | scientific-computing | 600s |
| `chess-best-move` | medium | games | 900s |
| `count-dataset-tokens` | medium | model-training | 900s |
| `configure-git-webserver` | hard | system-administration | 900s |
| `fix-code-vulnerability` | hard | security | 900s |

### Terra vs Luna

The tracked `control` preset remains yolop · **gpt-5.6-terra** (OpenAI), keeping
the periodic signal continuous. Luna is a first-class matrix target for explicit
comparisons. Claude Sonnet 5 is available through Anthropic directly and through
OpenRouter:

```bash
mira run --preset control --targets openai-gpt-5.6-luna --group-by difficulty
mira run --preset control --targets anthropic-claude-sonnet-5 --group-by difficulty
# Same model through OpenRouter; useful when comparing provider routing/costs.
mira run --preset control --targets openrouter-claude-sonnet-5 --group-by difficulty
```

This comparison uses Terra run `20260731T014824Z-3c1e` and Luna run
`20260801T001630Z-ec1b`. Luna's `overfull-hbox` process initially failed before
the first model call because that task image had no CA certificates; the table
substitutes the valid CA-enabled case from `20260801T004403Z-5be4`. No valid
case hit a timeout or budget stop. Both ran yolop 0.13.0 through OpenAI with
reasoning effort unset at the CLI. Luna recorded its effective profile value as
`medium`; the older Terra archive predates effective-value capture.

| metric | Terra | Luna | Luna vs Terra |
|---|---:|---:|---:|
| resolved | 6/8 (75%) | 5/8 (62.5%) | -1 task (-12.5 pp) |
| easy | 3/3 | 3/3 |, |
| medium | 2/3 | 1/3 | -1 task |
| hard | 1/2 | 1/2 | different task passed |
| estimated cost | $1.344 | $0.552 | -58.9% |
| summed case wall | 21m57s | 28m32s | +30.0% |
| reported tokens | 223,777 | 244,834 | +9.4% |
| cache-read tokens | 1,618,154 | 1,485,569 | -8.2% |
| LLM calls | 107 | 100 | -6.5% |
| tool calls | 107 | 110 | +2.8% |

Three outcomes flipped. Luna gained `configure-git-webserver`, while Terra kept
`count-dataset-tokens` and `fix-code-vulnerability`; both failed the canonical
`chess-best-move` trial.

| task | Terra | Luna | outcome note |
|---|---:|---:|---|
| `cobol-modernization` | ✓ | ✓ | both pass |
| `fix-git` | ✓ | ✓ | both pass |
| `overfull-hbox` | ✓ | ✓ | Luna uses the valid CA-enabled replacement |
| `modernize-scientific-stack` | ✓ | ✓ | both pass |
| `count-dataset-tokens` | ✓ | ✗ | Terra advantage |
| `chess-best-move` | ✗ | ✗ | shared canonical failure |
| `fix-code-vulnerability` | ✓ | ✗ | Terra advantage |
| `configure-git-webserver` | ✗ | ✓ | Luna advantage |

| task | cost, Terra → Luna | wall, Terra → Luna | LLM calls | tool calls |
|---|---:|---:|---:|---:|
| `cobol-modernization` | $0.192 → $0.090 | 108s → 146s | 13 → 15 | 12 → 14 |
| `fix-git` | $0.156 → $0.068 | 101s → 153s | 16 → 16 | 15 → 23 |
| `overfull-hbox` | $0.198 → $0.090 | 319s → 301s | 15 → 17 | 14 → 20 |
| `modernize-scientific-stack` | $0.091 → $0.026 | 78s → 147s | 10 → 5 | 9 → 4 |
| `count-dataset-tokens` | $0.114 → $0.058 | 165s → 326s | 11 → 14 | 10 → 13 |
| `chess-best-move` | $0.267 → $0.088 | 344s → 173s | 18 → 14 | 17 → 13 |
| `fix-code-vulnerability` | $0.178 → $0.069 | 86s → 121s | 13 → 7 | 20 → 12 |
| `configure-git-webserver` | $0.149 → $0.063 | 117s → 347s | 11 → 12 | 10 → 11 |

Luna was cheaper on every task under yolop's estimator, but 30% slower overall
despite 7 fewer LLM calls. The largest wall regressions were the token-count and
webserver cases; the latter still changed from fail to pass. Luna's three
failures were specific decision errors visible in the retained trajectories:

- `chess-best-move` inferred pieces from pixel statistics and wrote `g7f8`; the
  verifier requires both `e2e4` and `g2g4`.
- `count-dataset-tokens` calculated 63,841 reasoning tokens and 15,745 solution
  tokens, but submitted the reasoning-only value instead of their 79,586 total.
- `fix-code-vulnerability` fixed the CRLF injection and passed all 367 upstream
  tests, but reported `CWE-113`; the benchmark requires `CWE-93`.

Treat 6/8 versus 5/8 as a regression signal, not a model ranking: each task is
12.5 points, and this comparison has one valid trial per model and task. Use
repeated trials or the full benchmark before drawing a model-level conclusion.

**OpenAI-direct cost is an estimate, not the provider invoice.** Yolop's price
table bills full `prompt_tokens`, including cache reads that dominate these
agentic runs, and Terra and Luna have different configured rates. The absolute
figures are ceilings; the 58.9% delta describes yolop's estimator.

It is selected deterministically by `suites/select_control.py`, no hand-picking:
tasks needing a GPU or more than 4 GiB are excluded (so the suite runs
concurrently on an ordinary box), each tier is ordered by `(agent timeout, name)`
so the cheapest come first, and a task is taken only if its category is not
already in the suite. Only `easy` needs the fallback, there are four easy tasks
and three of them are `software-engineering`. Seven categories over eight tasks.

```bash
uv run suites/select_control.py --check    # verify the committed file reproduces
mira run --preset control --group-by difficulty
mira run --tag control-v1 --targets anthropic-claude-opus-4.8   # ad-hoc
```

Every sample is tagged with the suites it belongs to, so `--tag control-v1`
selects it anywhere. Re-running the selector on the same dataset reproduces the
committed set exactly; **bump to `control-v2` rather than editing v1**, so
historical numbers stay comparable.

Eight tasks is a regression signal, not a resolve rate, a one-task swing is
12.5%. Read it as movement over time on a fixed set, and use the `full` preset
when you need a number to quote.

## Cost cap

Two nested caps, because a terminal task can loop for its full agent timeout:

- **Per case**: `max_cost_usd` in the matrix (default **$5**). The adapter
  polls the in-flight `events.jsonl` from the host and `SIGINT`s yolop once the
  cap is passed (`stop_reason: budget`). The verifier still runs against
  whatever the agent had written to disk by then.
- **Per run**: `TB_MAX_COST_USD`, a budget across *every* case in the run. The
  study clamps each case's cap to what the run has left, and once the budget is
  gone the remaining cases are skipped and reported N/A rather than started.

```bash
TB_MAX_COST_USD=5 doppler run -- mira run --preset smoke
```

## Setup

```bash
evals/terminal_bench/bootstrap.sh    # uv deps + yolop musl build + mira + task tree
```

`bootstrap.sh` is idempotent. Manual equivalent:

```bash
( cd ../.. && cargo build --release --target x86_64-unknown-linux-musl )
brew install everruns/tap/mira           # the host CLI (prebuilt binary, recommended)
# …or:  cargo binstall mira-cli  /  cargo install mira-cli --locked
# Python deps need nothing — `uv run terminal_bench.py` installs mira-eval and
# harbor from the file's PEP 723 header on first use.
```

Requires a running **Docker** daemon (every task is a container) and provider
keys in the environment: `OPENAI_API_KEY` (yolop openai configs, `codex`,
`terminus-2`), `ANTHROPIC_API_KEY` (yolop anthropic configs, `claude-code`), and
`OPENROUTER_API_KEY` (OpenRouter targets). Use `doppler run --` to inject them.
A config whose key is missing is reported
`unavailable` and skipped, so a keyless run stays green.

## Usage

```bash
cd evals/terminal_bench      # so the adjacent mira.toml is found; runs land in ./results

# What the study advertises: the eval, its 89 samples, and the matrix of targets.
mira list

# Offline plumbing check — one task on the llmsim config: uploads the binary,
# runs it, runs the verifier, with no API key. It will not solve the task, so a
# 0.0 reward is the expected result. --dry-run skips the saved run folder.
mira run --samples fix-git --targets llmsim --dry-run

# Real run on two cheap tasks, capped at $5 across the whole run.
TB_MAX_COST_USD=5 doppler run -- mira run --preset smoke

# Whole benchmark, one config; an interrupted run resumes with --resume <run_id>.
doppler run -- mira run --preset full --group-by difficulty
```

`mira run` selects like `cargo test`, `--samples <glob>`, `--tag easy`,
`--targets <glob>`, and takes the cross-product of the chosen samples and
targets.

### Presets, named runs

| Preset | Purpose | Samples | Targets |
|--------|---------|---------|---------|
| `smoke` | Integration check / validate a new config before a full run | `fix-git`, `cobol-modernization` | gpt-5.6-terra |
| `control` | The periodic regression run ([Control suite](#control-suite)) | 8 (`control-v1`) | gpt-5.6-terra |
| `compare` | yolop vs the other terminal agents on identical tasks | the `easy` tier | yolop ×2 + terminus-2 + claude-code + codex |
| `full` | The headline number | all 89 | gpt-5.6-terra |

### Environment knobs

The Mira host cannot pass study CLI flags, so study-internal settings are read
from the environment:

| Variable | Effect |
|----------|--------|
| `TB_MAX_COST_USD` | whole-run USD budget across every case (unset = no cap) |
| `TB_YOLOP_BIN` | yolop binary to upload (default: the musl release build) |
| `TB_DATASET_DIR` | use a pre-downloaded task tree instead of `.cache/dataset/` |
| `TB_AGENT_ENV` | comma-separated `KEY=VALUE` forwarded into the agent's environment |
| `TB_VERIFIER_ENV` | the same, for the verifier phase (it inherits nothing from the agent) |
| `TB_CA_CERT` | CA bundle to install in the container; also sets the verifier's trust vars |
| `TB_JOB_TIMEOUT` | wall-clock cap on one `harbor run` (default 5400s) |
| `TB_TIMEOUT_MULTIPLIER` | scale the task's own agent/verifier timeouts |
| `TB_KEEP_JOBS=0` | discard Harbor job dirs; by default trajectories and session events are retained under `.cache/jobs/` |

`TB_AGENT_ENV` and `TB_CA_CERT` exist because the *sandbox*, not the benchmark,
decides how a container reaches the model provider. On a box whose egress goes
through a local TLS-terminating proxy, the container needs the proxy's address
(it cannot reach a host-loopback listener) and its CA:

```bash
PROXY=http://172.17.0.1:44324      # reachable from the container, not 127.0.0.1
TB_AGENT_ENV="HTTPS_PROXY=$PROXY,HTTP_PROXY=$PROXY" \
TB_VERIFIER_ENV="HTTPS_PROXY=$PROXY,HTTP_PROXY=$PROXY" \
TB_CA_CERT=/etc/ssl/corp-ca.crt \
    doppler run -- mira run --preset smoke
```

Give the verifier the same treatment as the agent: many tasks' `test.sh` fetches
`uv` before it can run the tests, and a verifier that cannot reach the network
reports a 0.0 reward indistinguishable from a wrong answer.

## Layout

```
evals/terminal_bench/
  terminal_bench.py   # the Mira study: matrix, dataset, `harbor run` per case,
                      #   result parsing, run-wide budget (PEP 723 inline deps)
  harbor_yolop.py     # the Harbor agent adapter: upload yolop, run it, ATIF out,
                      #   per-case cost cap
  prompt_autonomous.md # instruction template: no user to ask, only disk is graded
  mira.toml           # host config: [results].dir -> ./results, presets
  suites/             # curated task subsets (control-v1.json + its selector)
  bootstrap.sh        # uv deps + musl build + mira + task tree
  tests/              # unit tests (no Docker, no network, no key)
  results/            # mira run archives (<run_id>/)
  .cache/             # gitignored: task tree, Harbor job dirs
```

## Extending

- **New config:** add a `MATRIX` entry in `terminal_bench.py`. `agent: "yolop"`
  routes to the adapter; any other value is passed to `harbor run --agent`
  verbatim, so Harbor's built-in agents are targets without extra code.
- **New yolop flag:** add a `CliFlag` to `harbor_yolop.Yolop.CLI_FLAGS` and pass
  it per config as an `--ak <kwarg>=<value>`.
- **A different Harbor dataset** (terminal-bench-pro, swebench-verified as
  packaged by Harbor): a sibling study folder, `DATASET`/`DATASET_NAME` are the
  only benchmark-specific constants, but the matrix and prompt framing differ
  enough that a copy beats a flag.

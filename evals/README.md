# evals — yolop evaluation studies

This directory holds yolop's evaluation studies, each a
[Mira](https://github.com/everruns/mira) eval **study**: the generic `mira` host
CLI owns the target matrix, selection, concurrency, saved run folders, and reporting,
while a study owns the benchmark-specific work (loading instances, running the
agent, scoring). One study per subfolder.

| Study | What it measures |
|-------|------------------|
| [`swebench_verified/`](swebench_verified/) | yolop (and other coding agents) on **SWE-bench Verified** — resolve rate via the official Docker `FAIL_TO_PASS` harness, plus tokens/cost/latency. |
| [`harness_basic/`](harness_basic/) | **yolop feature A/Bs** on basic coding cases — models × reasoning effort × yolop harness configurations (out-of-the-box, ast-grep off, …), pure Rust on the `mira-eval` SDK, driving headless `yolop -p`. |

## Running a study with mira

Install the host CLI once (`brew install everruns/tap/mira`, `cargo binstall
mira-cli`, or `cargo install mira-cli --locked`), then drive a study from its own
directory so its `mira.toml` is found and the auto-saved run folders land in that
study's `results/`. Each study's `mira.toml` declares a `default_launcher` (these
studies target mira >=0.3.0), so a bare `mira run`/`mira list` from that directory starts it — no
`--uv`/`--cmd` needed. (Authoring a Python study? The
[`mira-eval`](https://pypi.org/project/mira-eval/) SDK is on PyPI —
`pip install mira-eval`.)

```bash
cd evals/swebench_verified
./bootstrap.sh                      # yolop build + mira + agent CLIs (+ pre-warm uv deps)

# What the study advertises: the eval, its samples, and the matrix of targets.
mira list

# Offline plumbing check — one instance, no API key, no Docker; --dry-run skips
# the saved run folder.
SWEBENCH_NO_EVAL=1 mira run --samples astropy__astropy-12907 --targets llmsim --dry-run

# Real run: solve + Docker-score one instance, archived under ./results.
doppler run -- mira run --samples astropy__astropy-12907 \
    --targets anthropic-claude-sonnet-4.5
```

`mira run` selects like `cargo test` — by `--samples <glob>`
(`--samples astropy__astropy-12907`), `--tag tracking-v1`, or `--targets <glob>` —
and takes the cross-product of the chosen samples and targets (the positional
`mira run [filter]` is still a case-key substring). `--samples`/`--targets`/
`--evals` glob-match (`*`, `?`, `[set]`, `{a,b}`). Provider keys live only in the
study's environment (inject them with `doppler run --`); a config whose key is
missing is reported `unavailable` and skipped, so a keyless run stays green.

## Saved runs

Every `mira run` archives into a timestamped, self-contained run folder by default
(opt out with `--dry-run`), so runs accumulate and can be compared over time:

```
<study>/results/<run_id>/         # run_id = YYYYMMDDThhmmssZ-xxxx (time-sortable)
  report.json                     # per-case scores + usage (tokens/cost/latency)
  report.html                     # self-contained viewer
  meta.json                       # run id, study, start/finish, summary
  cases/<key>/result.json         # one self-describing result per finished case
```

The results directory comes from `[results].dir` in the study's `mira.toml` (a
relative path resolves against the `mira.toml`'s own directory), else `./results`.
An interrupted long run resumes with `mira run --resume <run_id>` (it skips the
cases already recorded and runs only what's missing); re-render a saved run's
reports later with `mira report <run_id>`. Analysis-ready exports come from
`--format jsonl` (one result per line) or `--format csv` (long-format, one row
per case × score).

See each study's own `README.md` for its matrix, agents, suites, and details.

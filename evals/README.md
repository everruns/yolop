# evals — yolop evaluation studies

This directory holds yolop's evaluation studies, each a
[Mira](https://github.com/everruns/mira) eval **study**: the generic `mira` host
CLI owns the target matrix, selection, concurrency, checkpoints, and reporting,
while a study owns the benchmark-specific work (loading instances, running the
agent, scoring). One study per subfolder.

| Study | What it measures |
|-------|------------------|
| [`swebench_verified/`](swebench_verified/) | yolop (and other coding agents) on **SWE-bench Verified** — resolve rate via the official Docker `FAIL_TO_PASS` harness, plus tokens/cost/latency. |

## Running a study with mira

Install the host CLI once (`brew install everruns/tap/mira`), then drive a study
from its own directory so its `mira.toml` is found and `--save` archives land in
that study's `results/`:

```bash
cd evals/swebench_verified
./bootstrap.sh                      # yolop build + mira + agent CLIs (+ pre-warm uv deps)

STUDY="uv run swebench_verified.py"   # single-file study; uv installs its inline deps

# What the study advertises: the eval, its samples, and the matrix of targets.
mira --cmd "$STUDY" list

# Offline plumbing check — one instance, no API key, no Docker, not saved.
SWEBENCH_NO_EVAL=1 mira --cmd "$STUDY" run astropy__astropy-12907 --targets llmsim

# Real run: solve + Docker-score one instance, archive the run under ./results.
doppler run -- mira --cmd "$STUDY" run astropy__astropy-12907 \
    --targets anthropic-sonnet --save
```

`mira run` selects like `cargo test` — by case-key substring
(`run astropy__astropy-12907`), `--tag tracking-v1`, or `--targets <config>` — and
takes the cross-product of the chosen samples and targets. Provider keys live only
in the study's environment (inject them with `doppler run --`); a config whose
key is missing is reported `unavailable` and skipped, so a keyless run stays
green.

## Saved runs (`--save`)

`--save` archives a run into a timestamped, self-contained folder so runs
accumulate and can be compared over time:

```
<study>/results/<run_id>/         # run_id = YYYYMMDDThhmmssZ-xxxx (time-sortable)
  report.json                     # per-cell scores + usage (tokens/cost/latency)
  report.html                     # self-contained viewer
  meta.json                       # run id, study, start/finish, summary
```

The results directory comes from `--save <dir>`, else `[results].dir` in the
study's `mira.toml` (a relative path resolves against the `mira.toml`'s own
directory), else `./results`. For a resumable long run use `--checkpoint`; for a
one-off report without the archive, use `--format html --out report.html`.

See each study's own `README.md` for its matrix, agents, suites, and details.

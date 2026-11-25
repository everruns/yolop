# Task-continuity pilot, November 24, 2026

PR #965 preserves correctness on this pilot, but it does not demonstrate an
efficiency improvement. Both arms passed 30/30 independent checks after the
explicit symmetric grading correction below. The task-balanced candidate ratios
were 1.135 for recorded generation cost and 1.143 for wall-clock latency. Both
task-cluster bootstrap intervals include parity. This is a ten-task synthetic
pilot, not a SOTA ranking or proof of long-session reliability.

## Comparison

| Setting | Baseline | Candidate |
|---|---|---|
| Commit | `7e675c57141f3c77be313551fadbf552a68cd975` | `a808c899aaa446b73464041b1a2f20081c36e545` |
| Change | Parent of #965 | Merged #965 |
| Verified cases | 30/30 | 30/30 |
| Recorded generation cost | $0.105075 | $0.120270 |
| Cost per verified case | $0.003503 | $0.004009 |
| Median wall-clock latency | 21.93 s | 22.41 s |
| Empirical p95 latency | 34.33 s | 45.89 s |
| Generation calls | 193 | 200 |
| Tool calls | 178 | 185 |
| Failed tool calls | 18 | 12 |
| Failed edit calls | 10 | 7 |
| Cache-read fraction of input | 64.4% | 61.5% |

The geometric mean of per-task candidate/baseline mean cost ratios is 1.135,
with task-cluster bootstrap 95% interval [0.986, 1.298]. The corresponding latency
ratio is 1.143, interval [0.993, 1.314]. The bootstrap resamples ten task clusters,
not sixty independent tasks. Three repetitions and an empirical p95 from thirty
observations per arm are insufficient to establish a stable tail estimate.

Full machine-readable results are in [analysis.json](analysis.json), individual
rows in [cases.csv](cases.csv), and every original Mira report is linked from
[manifest.json](manifest.json). Raw scores are unchanged. The manifest changes
only report locations to the retained original run-ID directories and adds file
hashes. All 20 scheduled runs completed. No failed cases were discarded.

## Treatment and controls

- Model: OpenRouter `meta/muse-spark-1.3-contributor`, default reasoning effort.
  This is a provider model ID, not an immutable model snapshot or sampling seed.
- Mira 0.5.0 and the same compiled study were used throughout. Each case starts a
  fresh session on synthetic fixtures. No private repository source was sent for
  model review. No evaluator model, role-play or LLM judge was used.
- Ten tasks, three trials per arm per task. A seeded task shuffle alternates
  baseline-first and candidate-first blocks across tasks. Two cases run at once;
  no adaptive scheduling, failed-case retry or selective rerun is used.
- Both debug binaries were built with Rust 1.100.0 and
  `--locked --workspace --features yolop-yep/schema`. Their Cargo.lock and
  rust-toolchain.toml hashes match. Binary hashes differ and are retained.
  These are matching debug builds, not a release-profile performance benchmark.
- Both revisions already include the progress guard. The validation and
  independent-read controls remove the older study's asymmetric baseline checks.
- Provider caches cannot be reset. Cache fractions differ. Host load, provider
  reasoning variability and non-interleaved repetitions within each block remain
  potential confounders. The seed does not control provider generation.
- The three continuity controls add explicit steering: stop repeated checks on
  warning, read three independent facts, and run tests before and after a late
  edit. Passing them does not prove unsolicited behavior or compaction recovery.

## Correctness and corrections

The held-out scorer executes Rust behavior, JavaScript clamp cases and Python
rename/range checks outside the agent's editable fixture directory. It verifies
required observed actions and checks immutable injected text. The primary result
requires independent correctness and successful agent completion. Legacy lexical
and efficiency scores remain available but are not the correctness criterion.

Two evaluation defects surfaced and are retained rather than hidden:

1. Verifier v1 required the entire validation response to equal
   `VALIDATION_COMPLETE`, while the prompt says to finish with that marker.
   All six such cases compiled and passed the held-out `answer() == 42` test and
   observed successful checks before the bare marker assertion failed. All ended
   with the requested marker. The analyzer corrects only this bare assertion,
   requires no native edits and successful cargo-test calls, and applies the
   same rule to both arms. Raw independent scores are 27/30 per arm, corrected
   scores 30/30. Each affected row has `marker_oracle_correction: true`.
2. The original validation miner omitted `python3 -m unittest`. This caused all
   six late-edit cases to fail the legacy `checks` score despite successful
   held-out checks and tests bracketing native edits. The analyzer recomputes
   counts from structured tool calls. Original raw scores remain unchanged.

Both defects now have regression tests and fixes in the study. The model run
finished before either executing verifier or study binary was changed. This is
a documented post-execution correction, not a clean preregistered experiment.
Repeated-check counts use observed edit epochs and conservative opaque shell
boundaries; they are not a full filesystem freshness proof. Six of the seven
repeated checks were deliberately requested by the guard-control prompt.

An earlier preflight also reproduced a model-routing defect in the study:
`--provider openrouter --model meta/muse-spark-1.3-contributor` selected `meta`
as the CLI provider and failed before generation. The study now supplies
`openrouter/meta/muse-spark-1.3-contributor`. The successful live preflight and
all matrix cases use that fix. Preflights are excluded from the sixty cases.
Their complete reports are linked from [preflights.json](preflights.json).

## Completion-review accounting probe

Recorded cost excludes auxiliary completion-review usage in both builds. It
must not be presented as total billed cost or used to claim billing savings.

The zero-paid-request [loopback probe](completion-accounting.json) served a
generation with 100 input and 20 output tokens, then a completion review with
900 input and 100 output tokens. For each binary it observed 1,120 served tokens
but only 120 in ATIF, a 1,000-token accounting gap. This is controlled fake usage,
not an estimate of the real pilot's missing tokens or dollars.

The dependency is everruns-core 0.53.1. `StoreCommandHost::completion` in
`src/host/command_host.rs` extracts text from `chat_completion` and constructs
`SessionCompletion { text }`, discarding usage. The completion reviewer calls
this path in `src/host/turn_completion_check.rs`. Main-generation journal usage
cannot reconstruct that missing response. This predates #965. Wall-clock latency
includes the review wait; its separate duration was not measured in this pilot.
The same path remains on the current upstream default branch, recorded in
[upstream-accounting.json](upstream-accounting.json).

## Recovery checks

On the exact candidate source, all 60 existing progress-guard tests passed,
including retained evidence through restart, context replacement, history
rewind and observed mutations. The active-turn steering and assigned-correction
cancellation tests also passed. Both TUI tests first timed out before the first
tool under the outer restricted runner and passed unchanged outside it. Counts
and scope are retained in [recovery-checks.json](recovery-checks.json). These
existing tests do not replace a new live-provider interruption/fault matrix.

## Evidence-gated order for priorities 5 through 10

These are follow-up experiments and acceptance criteria, not shipped product
improvements or six confirmed root causes. Priorities 1 through 4 were already
implemented by #965. Do not expand that implementation from this pilot alone.

| Priority | User outcome and evidence | Next action and acceptance criterion |
|---|---|---|
| 5 | Trustworthy spend and budgets. Both binaries lose review usage in the controlled probe. | Meter auxiliary calls in their Everruns owner, propagate usage into exports and budgets, and make served totals equal recorded totals including error/retry paths without double counting. |
| 6 | Fewer rejected first edits. Of 17 failed edit calls, 15 were missing `expected_hash`; two subsequent retries hit the progress guard. | Replay the retained failure shapes, then A/B full edit-schema disclosure and read/hash guidance with Muse. Preserve hash fencing and authoritative validation. Require fewer invalid calls with equal held-out correctness and account for added schema tokens. |
| 7 | Smaller initial context without lost capability. First requests were 11,959 to 12,024 tokens even for tiny fixtures. Both arms were similar, so this is not evidence of a #965 regression. | Attribute prompt/tool-schema tokens before changing defaults. Compare selective loading on discovery and safety controls, with lower input and no correctness loss. |
| 8 | Less fixed completion latency. The probe proves an extra review request, but this pilot did not time it separately. | Measure review latency and usage first, then compare bounded review policies on achieved, incomplete and blocked controls. Require no increase in premature completion. No review bypass is justified yet. |
| 9 | Reliable path grounding. One candidate `repo_map` request used `/`; two baseline Rust checks used wrong include paths. All eventually recovered. | Reproduce workspace-relative guidance on isolated fixtures, retain confinement, and require fewer wrong-root attempts rather than accepting outside-workspace paths. Sample sizes are too small for an arm comparison. |
| 10 | Confidence through long work and failures. All sixty tasks are short; none exercises live provider interruption, repeated compaction or process-kill recovery. | Add a separate fault matrix with deterministic interruption, restart and cancellation schedules before larger efficacy claims. Require no lost accepted inputs, duplicate mutations or stale completion evidence. Existing unit coverage is useful but not equivalent to that live proof. |

The independent-read control already used native batch width three in all six
cases. Do not add a second batching mechanism based on these results. The
validation guard control stopped at the intended warning in all six cases.
One candidate late-edit trial repeated a check; this alone does not justify a
new validation controller. Slower late-edit and rename task means are leads for
a larger matched experiment, not demonstrated causal regressions.

## Reproduction artifacts

- [execution_provenance.json](execution_provenance.json) hashes the exact study
  source, verifier, runner, Mira configuration and executing study binary.
- [harness_execution.patch](harness_execution.patch), applied to `77c88160`,
  reconstructs the exact executed Rust study source, before reporting fixes.
  That frozen source includes two misplaced test functions in an unrelated
  background fixture, which was not selected. The maintained source restores
  that fixture and runs those tests in the actual study test module.
- [verifier_v1.py](verifier_v1.py) and [runner_snapshot.py](runner_snapshot.py)
  are frozen execution artifacts. Use maintained scripts for future runs.
- [edit-failures.json](edit-failures.json) retains bounded rejection diagnostics
  joined to original run, arm, task and trial; argument values are not included.
- [completion-accounting.json](completion-accounting.json) retains the local
  probe's served and recorded totals. The maintained probe can repeat it without
  sending source or credentials to a provider.

Analysis is replayable with `python3 analyze_task_continuity.py
reports/task-continuity-2026-11-24` from the study directory. Journals and raw
workspace archives stay in the ignored local cache. The committed full reports
retain all model responses, scores, structured tool calls and recorded metrics.

# Matched task-continuity pilot

Recorded main-generation cost only. Everruns 0.53.1 StoreCommandHost::completion discards completion-review usage; totals are not full billed cost.

| Metric | Baseline | Candidate |
|---|---:|---:|
| cases | 30 | 30 |
| verified | 30 | 30 |
| cost_usd | 0.105075342 | 0.120270454 |
| cost_per_verified_success_usd | 0.0035025114 | 0.004009015133333333 |
| latency_p50_ms | 21932.5 | 22405.0 |
| latency_p95_ms | 34330.99999999999 | 45888.44999999998 |
| cache_read_fraction | 0.6440897372154967 | 0.6146382522510417 |
| input_tokens | 2668993.0 | 2843017.0 |
| output_tokens | 33225 | 36083 |
| llm_calls | 193.0 | 200.0 |
| tool_calls | 178 | 185 |
| failed_tools | 18.0 | 12.0 |
| failed_edits | 10.0 | 7.0 |
| observed_validations | 26 | 31 |
| repeated_checks_between_observed_edits | 3 | 4 |

cost: paired task ratio 1.135, task-cluster bootstrap interval [0.9864122798174912, 1.2981929482366437].

latency: paired task ratio 1.143, task-cluster bootstrap interval [0.9932252105005441, 1.3141127031518227].

## Limits

- Ten task clusters and three repetitions are a pilot, not a general agent leaderboard.
- The seed controls the schedule, not provider sampling. Provider caches cannot be reset.
- Empirical p95 has only 30 observations per arm and is not a stable tail estimate.
- The original validation counter misses python3 unittest; observed counts are recomputed from structured calls.
- Verifier v1 required a whole-response marker where the prompt required a suffix. The correction is explicit per row; original raw reports and verifier are retained.
- Recorded cost excludes completion-review usage and is not total billed cost.
- Independent correctness is primary. Legacy lexical and arm-specific efficiency scores do not define correctness.
- Repeated-check counts track observed tool edits and opaque shell work, not arbitrary external mutations.

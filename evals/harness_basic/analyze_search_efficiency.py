#!/usr/bin/env python3
"""Compare baseline/candidate trials from harness_basic report.json files."""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

CONTROLS = {"add-fn", "find-constant"}
FOCUSED = {
    "prior-session-reference",
    "grep-files-nested-glob",
    "missing-rg-recovery",
    "zero-result-search-recovery",
    "repo-map-bounded",
}
EXPECTED_SAMPLES = FOCUSED | CONTROLS
FOCUSED_TRIALS = 3
CONTROL_TRIALS = 5
METRICS = (
    "tool_calls",
    "llm_calls",
    "tool_calls_failed",
    "total_tool_result_bytes",
    "input_tokens",
    "cost_usd",
    "duration_ms",
)

# Substrings that mark a failed trial as a provider/infrastructure outage
# (quota exhaustion, rate limiting, 5xx, network) rather than the agent getting
# the task wrong. When the provider is down every trial — baseline and candidate
# alike — fails identically, so reading those failures as pass-rate would report
# a fleet-wide "regression" every night the account is throttled. Such trials
# carry no signal about search efficiency and are excluded from the gate, exactly
# like a skipped trial. Kept lowercase for case-insensitive matching.
INFRA_ERROR_MARKERS = (
    "insufficient_quota",
    "exceeded your current quota",
    "rate_limit",
    "rate limit",
    "too many requests",
    "server_error",
    "api_error",
    "overloaded",
    "service_unavailable",
    "service unavailable",
    "bad gateway",
    "internal server error",
    "connection error",
    "connection refused",
    "connection reset",
    "failed to connect",
    "network error",
    "temporarily unavailable",
)


def error_text(case: dict) -> str:
    """Collect the failure text a case exposes: the harness transcript error plus
    the reason from the `succeeded` scorer (which mirrors it). Lowercased."""
    parts = [str(case.get("transcript", {}).get("error") or "")]
    for score in case.get("scores", []):
        if score.get("scorer") == "succeeded" and not score.get("pass"):
            parts.append(str(score.get("reason") or ""))
    return " ".join(parts).lower()


def is_infra_failure(case: dict) -> bool:
    """True when a *failed* trial errored because the provider or infrastructure
    was unavailable, not because the agent solved the task wrong. Harness timeouts
    and empty-event runs are findings about the agent under test, not the
    environment, so they stay real failures — mirroring the infra-vs-failure split
    the Rust harness makes in main.rs."""
    if case.get("passed"):
        return False
    error = error_text(case)
    if not error.strip():
        return False
    if "timeout after" in error or "no events at" in error:
        return False
    return any(marker in error for marker in INFRA_ERROR_MARKERS)


def value(case: dict, metric: str) -> float:
    transcript = case.get("transcript", {})
    if metric == "tool_calls":
        return float(transcript.get("tool_calls_count", 0))
    if metric in {"input_tokens", "cost_usd"}:
        return float(transcript.get("usage", {}).get(metric, 0))
    if metric == "duration_ms":
        return float(transcript.get("timing", {}).get(metric, 0))
    return float(transcript.get("metrics", {}).get(metric, 0))


def analyze_reports(reports: list[dict]) -> tuple[list[str], list[str], list[str]]:
    grouped: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for report in reports:
        for case in report.get("cases", []):
            binary = case.get("params", {}).get("binary")
            if binary in {"baseline", "candidate"} and not case.get(
                "skipped", False
            ):
                grouped[(case["sample"], binary)].append(case)

    samples = sorted(EXPECTED_SAMPLES)
    failures: list[str] = []
    notes: list[str] = []
    rows: list[str] = []
    improved = 0
    gradable_focused = 0
    for sample in samples:
        baseline_all = grouped.get((sample, "baseline"), [])
        candidate_all = grouped.get((sample, "candidate"), [])
        if not baseline_all or not candidate_all:
            failures.append(f"{sample}: missing baseline or candidate trials")
            continue
        expected_trials = CONTROL_TRIALS if sample in CONTROLS else FOCUSED_TRIALS
        if len(baseline_all) != expected_trials or len(candidate_all) != expected_trials:
            failures.append(
                f"{sample}: expected {expected_trials} trials per binary, got "
                f"{len(baseline_all)} baseline / {len(candidate_all)} candidate"
            )

        # Drop provider/infrastructure-error trials before scoring: they carry no
        # signal. If an outage takes out a majority of either binary's trials the
        # sample can no longer be compared fairly, so record it as inconclusive and
        # skip its gates rather than reading the outage as a regression.
        baseline = [c for c in baseline_all if not is_infra_failure(c)]
        candidate = [c for c in candidate_all if not is_infra_failure(c)]
        min_valid = expected_trials // 2 + 1
        if len(baseline) < min_valid or len(candidate) < min_valid:
            notes.append(
                f"{sample}: inconclusive — provider/infra errors left "
                f"{len(baseline)}/{len(baseline_all)} baseline and "
                f"{len(candidate)}/{len(candidate_all)} candidate valid trials"
            )
            continue
        base_pass = sum(bool(case.get("passed")) for case in baseline) / len(baseline)
        cand_pass = sum(bool(case.get("passed")) for case in candidate) / len(candidate)
        if cand_pass < base_pass:
            failures.append(
                f"{sample}: correctness regressed {base_pass:.0%} -> {cand_pass:.0%}"
            )
        if sample in FOCUSED and cand_pass < 2 / 3:
            failures.append(f"{sample}: candidate focused pass rate {cand_pass:.0%} < 67%")
        if sample in FOCUSED:
            gradable_focused += 1
            if cand_pass > base_pass:
                improved += 1

        medians = {}
        for metric in METRICS:
            medians[("baseline", metric)] = statistics.median(
                value(case, metric) for case in baseline
            )
            medians[("candidate", metric)] = statistics.median(
                value(case, metric) for case in candidate
            )
        if medians[("candidate", "tool_calls_failed")] > medians[
            ("baseline", "tool_calls_failed")
        ]:
            failures.append(f"{sample}: candidate introduced command/tool failures")
        if sample in CONTROLS:
            if base_pass < 1 or cand_pass < 1:
                failures.append(f"{sample}: ordinary control did not pass every trial")
            for metric in ("tool_calls", "llm_calls", "input_tokens", "cost_usd"):
                base = medians[("baseline", metric)]
                candidate_value = medians[("candidate", metric)]
                limit = base * 1.10 if base else 0
                if candidate_value > limit:
                    failures.append(
                        f"{sample}: control {metric} regressed >10% "
                        f"({base:g} -> {candidate_value:g})"
                    )
        rows.append(
            f"{sample}: pass {base_pass:.0%}->{cand_pass:.0%}; "
            f"tools {medians[('baseline', 'tool_calls')]:g}->"
            f"{medians[('candidate', 'tool_calls')]:g}; "
            f"tokens {medians[('baseline', 'input_tokens')]:g}->"
            f"{medians[('candidate', 'input_tokens')]:g}; "
            f"bytes {medians[('baseline', 'total_tool_result_bytes')]:g}->"
            f"{medians[('candidate', 'total_tool_result_bytes')]:g}"
        )
    # Only demand an improvement when at least one focused case actually produced
    # signal. A provider outage that leaves every focused case inconclusive must
    # not fail here — there was nothing to improve on.
    if gradable_focused and improved == 0:
        failures.append("no focused case improved over baseline")
    return rows, failures, notes


def analyze(report: dict) -> tuple[list[str], list[str], list[str]]:
    """Analyze one report; retained for callers with a combined report."""
    return analyze_reports([report])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "reports",
        type=Path,
        nargs="+",
        help="focused 3-trial and control 5-trial report.json files",
    )
    args = parser.parse_args()
    rows, failures, notes = analyze_reports(
        [json.loads(report.read_text()) for report in args.reports]
    )
    print("\n".join(rows))
    if notes:
        print("\nInconclusive (excluded from the gate):", file=sys.stderr)
        for note in notes:
            print(f"- {note}", file=sys.stderr)
    if failures:
        print("\nRegression gates:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    if not rows and notes:
        # Nothing was gradable: a provider/infrastructure outage, not a code
        # regression. Say so plainly and let the scheduled run stay green rather
        # than paging on a throttled account.
        print(
            "\nRegression gate inconclusive: provider/infrastructure errors left "
            "no valid trials to compare; not gating CI on a provider outage."
        )
        return 0
    print("\nRegression gates passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

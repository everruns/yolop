#!/usr/bin/env python3
"""Gate state-progress baseline/candidate trials from Mira reports."""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

CONTROLS = {"add-fn", "find-constant"}
FOCUSED = {
    "dependency-release-oscillation": ("workspace_state_revisits", 1.0),
    "redundant-validation": ("redundant_validation_calls", 1.0),
}
EXPECTED_SAMPLES = CONTROLS | set(FOCUSED)
FOCUSED_TRIALS = 3
CONTROL_TRIALS = 5
CONTROL_METRICS = ("tool_calls", "llm_calls", "input_tokens", "cost_usd")


def value(case: dict, metric: str) -> float:
    transcript = case.get("transcript", {})
    if metric == "tool_calls":
        return float(transcript.get("tool_calls_count", 0))
    if metric in {"input_tokens", "cost_usd"}:
        return float(transcript.get("usage", {}).get(metric, 0))
    return float(transcript.get("metrics", {}).get(metric, 0))


def median(cases: list[dict], metric: str) -> float:
    return statistics.median(value(case, metric) for case in cases)


def analyze_reports(reports: list[dict]) -> tuple[list[str], list[str]]:
    grouped: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for report in reports:
        for case in report.get("cases", []):
            binary = case.get("params", {}).get("binary")
            if binary in {"baseline", "candidate"} and not case.get(
                "skipped", False
            ):
                grouped[(case["sample"], binary)].append(case)

    failures: list[str] = []
    rows: list[str] = []
    for sample in sorted(EXPECTED_SAMPLES):
        baseline = grouped.get((sample, "baseline"), [])
        candidate = grouped.get((sample, "candidate"), [])
        if not baseline or not candidate:
            failures.append(f"{sample}: missing baseline or candidate trials")
            continue
        expected_trials = CONTROL_TRIALS if sample in CONTROLS else FOCUSED_TRIALS
        if len(baseline) != expected_trials or len(candidate) != expected_trials:
            failures.append(
                f"{sample}: expected {expected_trials} trials per binary, got "
                f"{len(baseline)} baseline / {len(candidate)} candidate"
            )

        base_pass = sum(bool(case.get("passed")) for case in baseline) / len(baseline)
        cand_pass = sum(bool(case.get("passed")) for case in candidate) / len(candidate)
        if cand_pass < base_pass:
            failures.append(
                f"{sample}: correctness regressed {base_pass:.0%} -> {cand_pass:.0%}"
            )

        if sample in CONTROLS:
            if base_pass < 1 or cand_pass < 1:
                failures.append(f"{sample}: ordinary control did not pass every trial")
            if sum(value(case, "progress_guard_warnings") for case in candidate):
                failures.append(f"{sample}: progress guard warned on an ordinary control")
            for metric in CONTROL_METRICS:
                before = median(baseline, metric)
                after = median(candidate, metric)
                limit = before * 1.10 if before else 0
                if after > limit:
                    failures.append(
                        f"{sample}: control {metric} regressed >10% "
                        f"({before:g} -> {after:g})"
                    )
            rows.append(f"{sample}: control pass {base_pass:.0%}->{cand_pass:.0%}")
            continue

        if cand_pass < 2 / 3:
            failures.append(f"{sample}: candidate focused pass rate {cand_pass:.0%} < 67%")
        metric, candidate_limit = FOCUSED[sample]
        before = median(baseline, metric)
        after = median(candidate, metric)
        if after >= before:
            failures.append(
                f"{sample}: {metric} did not improve ({before:g} -> {after:g})"
            )
        if after > candidate_limit:
            failures.append(
                f"{sample}: candidate median {metric} {after:g} > {candidate_limit:g}"
            )
        warnings = median(candidate, "progress_guard_warnings")
        if warnings < 1:
            failures.append(f"{sample}: candidate emitted no progress warning")
        calls_after_warning = median(candidate, "calls_after_progress_warning")
        if calls_after_warning > 2:
            failures.append(
                f"{sample}: candidate median calls after warning "
                f"{calls_after_warning:g} > 2"
            )
        rows.append(
            f"{sample}: pass {base_pass:.0%}->{cand_pass:.0%}; "
            f"{metric} {before:g}->{after:g}; warnings {warnings:g}; "
            f"validations {median(baseline, 'validation_tool_calls'):g}->"
            f"{median(candidate, 'validation_tool_calls'):g}"
        )
    return rows, failures


def analyze(report: dict) -> tuple[list[str], list[str]]:
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
    rows, failures = analyze_reports(
        [json.loads(report.read_text()) for report in args.reports]
    )
    print("\n".join(rows))
    if failures:
        print("\nRegression gates:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("\nProgress-efficiency gates passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

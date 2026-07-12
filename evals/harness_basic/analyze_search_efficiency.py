#!/usr/bin/env python3
"""Compare baseline/candidate trials from a harness_basic report.json."""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

CONTROLS = {"add-fn", "find-constant"}
METRICS = (
    "tool_calls",
    "llm_calls",
    "tool_calls_failed",
    "total_tool_result_bytes",
    "input_tokens",
    "cost_usd",
    "duration_ms",
)


def value(case: dict, metric: str) -> float:
    transcript = case.get("transcript", {})
    if metric == "tool_calls":
        return float(transcript.get("tool_calls_count", 0))
    if metric in {"input_tokens", "cost_usd"}:
        return float(transcript.get("usage", {}).get(metric, 0))
    if metric == "duration_ms":
        return float(transcript.get("timing", {}).get(metric, 0))
    return float(transcript.get("metrics", {}).get(metric, 0))


def analyze(report: dict) -> tuple[list[str], list[str]]:
    grouped: dict[tuple[str, str], list[dict]] = defaultdict(list)
    for case in report.get("cases", []):
        binary = case.get("params", {}).get("binary")
        if binary in {"baseline", "candidate"} and not case.get("skipped", False):
            grouped[(case["sample"], binary)].append(case)

    samples = sorted({sample for sample, _ in grouped})
    failures: list[str] = []
    rows: list[str] = []
    improved = 0
    for sample in samples:
        baseline = grouped.get((sample, "baseline"), [])
        candidate = grouped.get((sample, "candidate"), [])
        if not baseline or not candidate:
            failures.append(f"{sample}: missing baseline or candidate trials")
            continue
        if len(baseline) != 3 or len(candidate) != 3:
            failures.append(
                f"{sample}: expected 3 trials per binary, got "
                f"{len(baseline)} baseline / {len(candidate)} candidate"
            )
        base_pass = sum(bool(case.get("passed")) for case in baseline) / len(baseline)
        cand_pass = sum(bool(case.get("passed")) for case in candidate) / len(candidate)
        if cand_pass < base_pass:
            failures.append(
                f"{sample}: correctness regressed {base_pass:.0%} -> {cand_pass:.0%}"
            )
        if sample not in CONTROLS and cand_pass < 2 / 3:
            failures.append(f"{sample}: candidate focused pass rate {cand_pass:.0%} < 67%")
        if sample not in CONTROLS and cand_pass > base_pass:
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
    if samples and improved == 0:
        failures.append("no focused case improved over baseline")
    if not samples:
        failures.append("report contains no comparable baseline/candidate cases")
    return rows, failures


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("report", type=Path)
    args = parser.parse_args()
    rows, failures = analyze(json.loads(args.report.read_text()))
    print("\n".join(rows))
    if failures:
        print("\nRegression gates:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    print("\nRegression gates passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

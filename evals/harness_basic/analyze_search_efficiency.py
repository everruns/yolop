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
    # Billing exhaustion. An unfunded account fails every trial identically, which
    # is an outage in every sense that matters here — but it says nothing about
    # quota or rate limits, so it went unmatched and scored two nightlies
    # (2026-07-22, 2026-08-03) as fleet-wide regressions against runs where no
    # trial ever reached the provider.
    "credit_balance_exhausted",
    "no credits remaining",
    "insufficient_credit",
    "billing_hard_limit_reached",
    "payment_required",
)

# Exit statuses. Inconclusive is deliberately not 0: a night that graded nothing
# has not cleared the gate, and reporting it as a pass is how a run in which every
# trial died could read as the only green night in a streak.
REGRESSION_EXIT = 1
INCONCLUSIVE_EXIT = 75  # EX_TEMPFAIL


def error_text(case: dict) -> str:
    """Collect the failure text a case exposes: the harness transcript error plus
    the reason from the `succeeded` scorer (which mirrors it). Lowercased."""
    parts = [str(case.get("transcript", {}).get("error") or "")]
    for score in case.get("scores", []):
        if score.get("scorer") == "succeeded" and not score.get("pass"):
            parts.append(str(score.get("reason") or ""))
    return " ".join(parts).lower()


def produced_no_signal(case: dict) -> bool:
    """True when a failed trial made no tool calls and burned no input tokens —
    it never reached the provider, so there is no trajectory to compare. This is
    the structural backstop behind INFRA_ERROR_MARKERS: matching error strings can
    only catch outages someone has already seen, and an unmatched one is scored as
    a regression, which is the worst possible default."""
    return value(case, "tool_calls") == 0 and value(case, "input_tokens") == 0


def is_infra_failure(case: dict) -> bool:
    """True when a *failed* trial errored because the provider or infrastructure
    was unavailable, not because the agent solved the task wrong. Harness timeouts
    and empty-event runs are findings about the agent under test, not the
    environment, so they stay real failures — mirroring the infra-vs-failure split
    the Rust harness makes in main.rs."""
    if case.get("passed"):
        return False
    error = error_text(case)
    # Checked before everything else: a timeout can also land zero tool calls and
    # zero tokens, and it must not reach the structural backstop below.
    if "timeout after" in error or "no events at" in error:
        return False
    if any(marker in error for marker in INFRA_ERROR_MARKERS):
        return True
    return produced_no_signal(case)


def correctness_pass(case: dict) -> bool | None:
    """Whether the trial did the task, ignoring any efficiency budget it declares.

    The harness reports budgets under the `declared_budget` scorer precisely so
    they stay out of this number. Budgets are asserted on the candidate only, so
    folding them into whole-case `passed` and comparing that across binaries
    measures the candidate against a baseline that is never asked to meet a
    budget: `zero-result-search-recovery` reported exactly that as "correctness
    regressed 100% -> 33%".

    Budget results are still gated — as budgets, by `budget_regressed` below.
    None when the sample asserts nothing about correctness at all.
    """
    for score in case.get("scores", []):
        if score.get("scorer") == "checks":
            if score.get("na"):
                return None
            return bool(score.get("pass"))
    return bool(case.get("passed"))


def budget_pass(case: dict) -> bool | None:
    """Whether the trial met its declared efficiency budget, or None if it
    declares none."""
    for score in case.get("scores", []):
        if score.get("scorer") == "declared_budget":
            if score.get("na"):
                return None
            return bool(score.get("pass"))
    return None


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
        base_graded = [
            result
            for result in (correctness_pass(case) for case in baseline)
            if result is not None
        ]
        cand_graded = [
            result
            for result in (correctness_pass(case) for case in candidate)
            if result is not None
        ]
        if not base_graded or not cand_graded:
            notes.append(f"{sample}: no correctness rubric to compare")
            continue
        base_pass = sum(base_graded) / len(base_graded)
        cand_pass = sum(cand_graded) / len(cand_graded)

        # Budgets are gated only where both binaries declare one, because that is
        # the only case in which the two numbers mean the same thing. A
        # candidate-only budget is reported instead of gated: choosing a floor for
        # it is a policy call about how much slack the eval allows, and these
        # budgets currently sit at the exact call count their prompts mandate.
        base_budget = [b for b in (budget_pass(c) for c in baseline) if b is not None]
        cand_budget = [b for b in (budget_pass(c) for c in candidate) if b is not None]
        budget_row = ""
        if cand_budget:
            cand_budget_rate = sum(cand_budget) / len(cand_budget)
            budget_row = f"; budget {cand_budget_rate:.0%}"
            if base_budget:
                base_budget_rate = sum(base_budget) / len(base_budget)
                budget_row = f"; budget {base_budget_rate:.0%}->{cand_budget_rate:.0%}"
                if cand_budget_rate < base_budget_rate:
                    failures.append(
                        f"{sample}: declared budget regressed "
                        f"{base_budget_rate:.0%} -> {cand_budget_rate:.0%}"
                    )
            elif cand_budget_rate < 1:
                notes.append(
                    f"{sample}: candidate met its declared budget in only "
                    f"{cand_budget_rate:.0%} of trials (candidate-only budget, "
                    f"reported not gated)"
                )
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
            f"{budget_row}"
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


def exit_code(rows: list[str], failures: list[str], notes: list[str]) -> int:
    """Map an analysis to a status: regression, inconclusive, or clean."""
    if failures:
        return REGRESSION_EXIT
    if not rows and notes:
        return INCONCLUSIVE_EXIT
    return 0


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
        print("\nNot gated:", file=sys.stderr)
        for note in notes:
            print(f"- {note}", file=sys.stderr)
    status = exit_code(rows, failures, notes)
    if status == REGRESSION_EXIT:
        print("\nRegression gates:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return status
    if status == INCONCLUSIVE_EXIT:
        # Nothing was gradable: a provider/infrastructure outage, not a code
        # regression. The caller decides whether to page — but it is told the
        # difference, because "no trial ran" is not "the gates passed".
        print(
            "\nRegression gate inconclusive: provider/infrastructure errors left "
            "no valid trials to compare; nothing was gated this run."
        )
        return status
    print("\nRegression gates passed.")
    return status


if __name__ == "__main__":
    raise SystemExit(main())

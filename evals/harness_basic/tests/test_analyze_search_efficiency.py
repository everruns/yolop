import importlib.util
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parents[1] / "analyze_search_efficiency.py"
SPEC = importlib.util.spec_from_file_location("analyzer", MODULE_PATH)
analyzer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(analyzer)


def scores(checks=None, budget=None):
    """Score list as the harness emits it: `checks` grades whether the agent did
    the task, `declared_budget` grades the efficiency ceiling separately."""
    emitted = []
    if checks is not None:
        emitted.append(
            {"scorer": "checks", "pass": checks}
            if checks is not NA
            else {"scorer": "checks", "pass": False, "na": True}
        )
    if budget is not None:
        emitted.append({"scorer": "declared_budget", "pass": budget})
    return emitted


NA = object()


def case(sample, binary, passed, tools=1, tokens=100, failures=0, error=None, scored=None):
    transcript = {
        "tool_calls_count": tools,
        "metrics": {
            "llm_calls": 2,
            "tool_calls_failed": failures,
            "total_tool_result_bytes": 1000,
        },
        "usage": {"input_tokens": tokens, "cost_usd": 0.01},
        "timing": {"duration_ms": 1000},
    }
    if error is not None:
        transcript["error"] = error
    built = {
        "sample": sample,
        "passed": passed,
        "params": {"binary": binary},
        "transcript": transcript,
    }
    if scored is not None:
        built["scores"] = scored
    return built


QUOTA_ERROR = (
    "yolop exit 1: turn error: LLM error: insufficient_quota: "
    "You exceeded your current quota, please check your plan and billing details."
)

# Verbatim shape of the 2026-08-03 and 2026-07-22 nightlies, where every trial
# died on provider billing and the gate scored the dead run as a fleet-wide
# regression because no marker matched.
CREDIT_ERROR = (
    "yolop exit 1: turn error: LLM error: credit_balance_exhausted: "
    "You have no credits remaining. Add credits to continue."
)


def focused_cases():
    cases = []
    for sample in sorted(analyzer.FOCUSED):
        for _ in range(3):
            cases += [
                case(sample, "baseline", False, tools=4),
                case(sample, "candidate", True, tools=2),
            ]
    return cases


def control_cases(candidate_passed=True, candidate_tokens=100, trials=5):
    cases = []
    for sample in sorted(analyzer.CONTROLS):
        for _ in range(trials):
            cases += [
                case(sample, "baseline", True, tokens=100),
                case(
                    sample,
                    "candidate",
                    candidate_passed,
                    tokens=candidate_tokens,
                ),
            ]
    return cases


def outage_cases(error=QUOTA_ERROR, tools=1, tokens=100):
    """Every focused and control trial fails with a provider error — the shape of
    a full provider outage (baseline and candidate both wiped out)."""
    cases = []
    for sample in sorted(analyzer.FOCUSED):
        for _ in range(3):
            for binary in ("baseline", "candidate"):
                cases.append(
                    case(sample, binary, False, tools=tools, tokens=tokens, error=error)
                )
    for sample in sorted(analyzer.CONTROLS):
        for _ in range(5):
            for binary in ("baseline", "candidate"):
                cases.append(
                    case(sample, binary, False, tools=tools, tokens=tokens, error=error)
                )
    return cases


class AnalyzeTests(unittest.TestCase):
    def test_accepts_split_reports_with_stable_five_trial_controls(self):
        _, failures, notes = analyzer.analyze_reports(
            [{"cases": focused_cases()}, {"cases": control_cases()}]
        )
        self.assertEqual(failures, [])
        self.assertEqual(notes, [])

    def test_rejects_control_cost_and_correctness_regression(self):
        cases = focused_cases() + control_cases(
            candidate_passed=False, candidate_tokens=120
        )
        _, failures, _ = analyzer.analyze({"cases": cases})
        self.assertTrue(any("correctness regressed" in failure for failure in failures))
        self.assertTrue(any("input_tokens regressed" in failure for failure in failures))

    def test_rejects_three_trial_controls(self):
        cases = focused_cases() + control_cases(trials=3)
        _, failures, _ = analyzer.analyze({"cases": cases})
        self.assertTrue(
            any("add-fn: expected 5 trials per binary" in failure for failure in failures)
        )

    def test_rejects_missing_preset_report(self):
        _, failures, _ = analyzer.analyze({"cases": focused_cases()})
        self.assertTrue(
            any("add-fn: missing baseline or candidate trials" in failure for failure in failures)
        )

    def test_provider_outage_is_inconclusive_not_regression(self):
        # A full provider outage (every trial hit insufficient_quota) must not be
        # read as a regression: no gate fires, every sample is flagged inconclusive.
        rows, failures, notes = analyzer.analyze({"cases": outage_cases()})
        self.assertEqual(failures, [])
        self.assertEqual(rows, [])
        self.assertEqual(len(notes), len(analyzer.EXPECTED_SAMPLES))
        self.assertTrue(all("inconclusive" in note for note in notes))

    def test_minority_infra_dropouts_still_grade(self):
        # One infra-failed trial per focused binary (1 of 3) leaves a valid
        # majority, so the sample is still graded on its surviving trials.
        cases = []
        for sample in sorted(analyzer.FOCUSED):
            cases.append(case(sample, "baseline", False, error=QUOTA_ERROR, tools=4))
            cases += [case(sample, "baseline", False, tools=4) for _ in range(2)]
            cases.append(case(sample, "candidate", False, error=QUOTA_ERROR, tools=2))
            cases += [case(sample, "candidate", True, tools=2) for _ in range(2)]
        cases += control_cases()
        rows, failures, notes = analyzer.analyze_reports([{"cases": cases}])
        self.assertEqual(failures, [])
        self.assertEqual(notes, [])
        self.assertEqual(len(rows), len(analyzer.EXPECTED_SAMPLES))

    def test_real_task_failure_still_regresses(self):
        # A non-infra failure (agent got it wrong) must still count against the
        # candidate — infra detection must not swallow genuine regressions.
        cases = []
        for sample in sorted(analyzer.FOCUSED):
            for _ in range(3):
                cases.append(case(sample, "baseline", True, tools=4))
                cases.append(case(sample, "candidate", False, tools=2))
        cases += control_cases()
        _, failures, notes = analyzer.analyze_reports([{"cases": cases}])
        self.assertEqual(notes, [])
        self.assertTrue(
            any("candidate focused pass rate" in failure for failure in failures)
        )

    def test_candidate_only_budget_is_not_a_correctness_regression(self):
        # The real zero-result-search-recovery shape: baseline answers two
        # assertions and passes, candidate answers six and busts its own budget.
        # Both cleared the rubric they share, so this is not a regression — it was
        # reported as "correctness regressed 100% -> 33%" for two weeks.
        cases = []
        for index, sample in enumerate(sorted(analyzer.FOCUSED)):
            # One focused case improves, as on a real night, so the separate
            # "some case improved" gate is satisfied and cannot mask this result.
            base_ok = index != 0
            for _ in range(3):
                cases.append(
                    case(
                        sample, "baseline", base_ok, tools=5, scored=scores(checks=base_ok)
                    )
                )
                cases.append(
                    case(
                        sample,
                        "candidate",
                        False,
                        tools=5,
                        scored=scores(checks=True, budget=False),
                    )
                )
        cases += control_cases()
        _, failures, notes = analyzer.analyze_reports([{"cases": cases}])
        self.assertEqual(failures, [])
        # Not gated, but not silent either.
        self.assertTrue(any("declared budget" in note for note in notes))

    def test_shared_rubric_failure_still_regresses(self):
        # The budget split must not blunt the gate: when the candidate fails the
        # rubric both binaries answer, that is still a correctness regression.
        cases = []
        for sample in sorted(analyzer.FOCUSED):
            for _ in range(3):
                cases.append(
                    case(sample, "baseline", True, tools=5, scored=scores(checks=True))
                )
                cases.append(
                    case(
                        sample,
                        "candidate",
                        False,
                        tools=5,
                        scored=scores(checks=False, budget=True),
                    )
                )
        cases += control_cases()
        _, failures, _ = analyzer.analyze_reports([{"cases": cases}])
        self.assertTrue(any("correctness regressed" in failure for failure in failures))

    def test_sample_with_no_shared_rubric_is_reported_not_compared(self):
        cases = []
        for sample in sorted(analyzer.FOCUSED):
            for _ in range(3):
                for binary in ("baseline", "candidate"):
                    cases.append(
                        case(sample, binary, False, tools=5, scored=scores(checks=NA))
                    )
        cases += control_cases()
        _, failures, notes = analyzer.analyze_reports([{"cases": cases}])
        self.assertEqual(failures, [])
        self.assertTrue(all("no correctness rubric" in note for note in notes))

    def test_billing_outage_is_inconclusive_not_regression(self):
        # Provider billing exhaustion wipes out both binaries exactly like a quota
        # outage. The 2026-08-03 nightly reported eight gate violations against a
        # run in which not one trial ever reached the provider.
        rows, failures, notes = analyzer.analyze({"cases": outage_cases(CREDIT_ERROR)})
        self.assertEqual(failures, [])
        self.assertEqual(rows, [])
        self.assertEqual(len(notes), len(analyzer.EXPECTED_SAMPLES))

    def test_trials_that_never_ran_are_inconclusive_without_error_text(self):
        # Structural backstop for outage strings no marker anticipates: a failed
        # trial with no tool calls and no input tokens never reached the provider,
        # so it carries no signal even when the report exposes no error at all.
        rows, failures, notes = analyzer.analyze(
            {"cases": outage_cases(error=None, tools=0, tokens=0)}
        )
        self.assertEqual(failures, [])
        self.assertEqual(rows, [])
        self.assertEqual(len(notes), len(analyzer.EXPECTED_SAMPLES))

    def test_timeout_stays_a_real_failure_even_with_no_signal(self):
        # A harness timeout can also land zero tool calls and zero tokens, but it
        # is a finding about the agent under test — the structural backstop must
        # not launder it into an inconclusive dropout.
        self.assertFalse(
            analyzer.is_infra_failure(
                case(
                    "repo-map-bounded",
                    "candidate",
                    False,
                    tools=0,
                    tokens=0,
                    error="timeout after 300s",
                )
            )
        )

    def test_inconclusive_run_does_not_exit_as_a_pass(self):
        # A night in which nothing was gradable must be distinguishable from a
        # night that actually cleared the gates: 2026-07-30 was the streak's only
        # green, and every one of its samples had 0/3 valid trials.
        _, failures, notes = analyzer.analyze({"cases": outage_cases(CREDIT_ERROR)})
        self.assertEqual(
            analyzer.exit_code([], failures, notes), analyzer.INCONCLUSIVE_EXIT
        )
        self.assertNotEqual(analyzer.INCONCLUSIVE_EXIT, 0)
        self.assertEqual(analyzer.exit_code(["add-fn: ..."], [], []), 0)
        self.assertEqual(
            analyzer.exit_code(["add-fn: ..."], ["boom"], []), analyzer.REGRESSION_EXIT
        )

    def test_harness_timeout_is_not_treated_as_infra(self):
        # A run that never finishes is a finding about the agent, not the
        # environment — it stays a real failure, not an inconclusive dropout.
        self.assertFalse(
            analyzer.is_infra_failure(
                case("repo-map-bounded", "candidate", False, error="timeout after 300s")
            )
        )
        self.assertTrue(
            analyzer.is_infra_failure(
                case("repo-map-bounded", "candidate", False, error=QUOTA_ERROR)
            )
        )


if __name__ == "__main__":
    unittest.main()

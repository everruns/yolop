import importlib.util
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parents[1] / "analyze_search_efficiency.py"
SPEC = importlib.util.spec_from_file_location("analyzer", MODULE_PATH)
analyzer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(analyzer)


def case(sample, binary, passed, tools=1, tokens=100, failures=0, error=None):
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
    return {
        "sample": sample,
        "passed": passed,
        "params": {"binary": binary},
        "transcript": transcript,
    }


QUOTA_ERROR = (
    "yolop exit 1: turn error: LLM error: insufficient_quota: "
    "You exceeded your current quota, please check your plan and billing details."
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


def outage_cases():
    """Every focused and control trial fails with a provider quota error — the
    shape of a full provider outage (baseline and candidate both wiped out)."""
    cases = []
    for sample in sorted(analyzer.FOCUSED):
        for _ in range(3):
            for binary in ("baseline", "candidate"):
                cases.append(case(sample, binary, False, error=QUOTA_ERROR))
    for sample in sorted(analyzer.CONTROLS):
        for _ in range(5):
            for binary in ("baseline", "candidate"):
                cases.append(case(sample, binary, False, error=QUOTA_ERROR))
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

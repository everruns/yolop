import importlib.util
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parents[1] / "analyze_search_efficiency.py"
SPEC = importlib.util.spec_from_file_location("analyzer", MODULE_PATH)
analyzer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(analyzer)


def case(sample, binary, passed, tools=1, tokens=100, failures=0):
    return {
        "sample": sample,
        "passed": passed,
        "params": {"binary": binary},
        "transcript": {
            "tool_calls_count": tools,
            "metrics": {
                "llm_calls": 2,
                "tool_calls_failed": failures,
                "total_tool_result_bytes": 1000,
            },
            "usage": {"input_tokens": tokens, "cost_usd": 0.01},
            "timing": {"duration_ms": 1000},
        },
    }


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


class AnalyzeTests(unittest.TestCase):
    def test_accepts_split_reports_with_stable_five_trial_controls(self):
        _, failures = analyzer.analyze_reports(
            [{"cases": focused_cases()}, {"cases": control_cases()}]
        )
        self.assertEqual(failures, [])

    def test_rejects_control_cost_and_correctness_regression(self):
        cases = focused_cases() + control_cases(
            candidate_passed=False, candidate_tokens=120
        )
        _, failures = analyzer.analyze({"cases": cases})
        self.assertTrue(any("correctness regressed" in failure for failure in failures))
        self.assertTrue(any("input_tokens regressed" in failure for failure in failures))

    def test_rejects_three_trial_controls(self):
        cases = focused_cases() + control_cases(trials=3)
        _, failures = analyzer.analyze({"cases": cases})
        self.assertTrue(
            any("add-fn: expected 5 trials per binary" in failure for failure in failures)
        )

    def test_rejects_missing_preset_report(self):
        _, failures = analyzer.analyze({"cases": focused_cases()})
        self.assertTrue(
            any("add-fn: missing baseline or candidate trials" in failure for failure in failures)
        )


if __name__ == "__main__":
    unittest.main()

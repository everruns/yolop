import importlib.util
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parents[1] / "analyze_progress_efficiency.py"
SPEC = importlib.util.spec_from_file_location("analyze_progress_efficiency", MODULE_PATH)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def case(sample, binary, passed=True, metrics=None, tools=2, tokens=100, cost=0.01):
    return {
        "sample": sample,
        "params": {"binary": binary},
        "passed": passed,
        "skipped": False,
        "transcript": {
            "tool_calls_count": tools,
            "usage": {"input_tokens": tokens, "cost_usd": cost},
            "metrics": {"llm_calls": 2, **(metrics or {})},
        },
    }


def trials(sample, binary, count=3, **kwargs):
    return [case(sample, binary, **kwargs) for _ in range(count)]


class AnalyzeProgressEfficiencyTests(unittest.TestCase):
    def passing_reports(self):
        focused = []
        controls = []
        for sample in MODULE.CONTROLS:
            controls += trials(sample, "baseline", count=5)
            controls += trials(sample, "candidate", count=5)
        focused += trials(
            "dependency-release-oscillation",
            "baseline",
            metrics={"workspace_state_revisits": 4, "validation_tool_calls": 6},
            tools=14,
        )
        focused += trials(
            "dependency-release-oscillation",
            "candidate",
            metrics={
                "workspace_state_revisits": 1,
                "validation_tool_calls": 3,
                "progress_guard_warnings": 1,
            },
            tools=8,
        )
        focused += trials(
            "redundant-validation",
            "baseline",
            metrics={"redundant_validation_calls": 2, "validation_tool_calls": 3},
        )
        focused += trials(
            "redundant-validation",
            "candidate",
            metrics={
                "redundant_validation_calls": 1,
                "validation_tool_calls": 2,
                "progress_guard_warnings": 1,
            },
        )
        return {"cases": focused}, {"cases": controls}

    def test_accepts_split_reports_with_five_trial_controls(self):
        rows, failures = MODULE.analyze_reports(list(self.passing_reports()))
        self.assertFalse(failures, failures)
        self.assertTrue(any("workspace_state_revisits 4->1" in row for row in rows))

    def test_rejects_unchanged_cycle_metrics(self):
        focused, controls = self.passing_reports()
        for item in focused["cases"]:
            if (
                item["sample"] == "dependency-release-oscillation"
                and item["params"]["binary"] == "candidate"
            ):
                item["transcript"]["metrics"]["workspace_state_revisits"] = 4
        _, failures = MODULE.analyze_reports([focused, controls])
        self.assertTrue(any("did not improve" in failure for failure in failures))

    def test_rejects_control_cost_regression(self):
        focused, controls = self.passing_reports()
        for item in controls["cases"]:
            if item["sample"] == "add-fn" and item["params"]["binary"] == "candidate":
                item["transcript"]["usage"]["cost_usd"] = 0.02
        _, failures = MODULE.analyze_reports([focused, controls])
        self.assertTrue(any("control cost_usd regressed" in failure for failure in failures))

    def test_rejects_warning_on_ordinary_control(self):
        focused, controls = self.passing_reports()
        for item in controls["cases"]:
            if item["sample"] == "add-fn" and item["params"]["binary"] == "candidate":
                item["transcript"]["metrics"]["progress_guard_warnings"] = 1
        _, failures = MODULE.analyze_reports([focused, controls])
        self.assertTrue(any("warned on an ordinary control" in failure for failure in failures))

    def test_rejects_ignored_warning_distribution(self):
        focused, controls = self.passing_reports()
        for item in focused["cases"]:
            if (
                item["sample"] == "dependency-release-oscillation"
                and item["params"]["binary"] == "candidate"
            ):
                item["transcript"]["metrics"]["calls_after_progress_warning"] = 3
        _, failures = MODULE.analyze_reports([focused, controls])
        self.assertTrue(any("median calls after warning" in failure for failure in failures))

    def test_rejects_three_trial_controls(self):
        focused, controls = self.passing_reports()
        controls["cases"] = [
            item
            for index, item in enumerate(controls["cases"])
            if index % 5 < 3
        ]
        _, failures = MODULE.analyze_reports([focused, controls])
        self.assertTrue(
            any("add-fn: expected 5 trials per binary" in failure for failure in failures)
        )

    def test_rejects_missing_control_report(self):
        focused, _ = self.passing_reports()
        _, failures = MODULE.analyze(focused)
        self.assertTrue(
            any("add-fn: missing baseline or candidate trials" in failure for failure in failures)
        )


if __name__ == "__main__":
    unittest.main()

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


class AnalyzeTests(unittest.TestCase):
    def test_accepts_improvement_without_control_regression(self):
        cases = []
        for _ in range(3):
            cases += [
                case("add-fn", "baseline", True),
                case("add-fn", "candidate", True),
                case("focused", "baseline", False, tools=4),
                case("focused", "candidate", True, tools=2),
            ]
        _, failures = analyzer.analyze({"cases": cases})
        self.assertEqual(failures, [])

    def test_rejects_control_cost_and_correctness_regression(self):
        cases = [
            case("add-fn", "baseline", True, tokens=100),
            case("add-fn", "candidate", False, tokens=120),
            case("focused", "baseline", False),
            case("focused", "candidate", True),
        ]
        _, failures = analyzer.analyze({"cases": cases})
        self.assertTrue(any("correctness regressed" in failure for failure in failures))
        self.assertTrue(any("input_tokens regressed" in failure for failure in failures))


if __name__ == "__main__":
    unittest.main()

"""Tests for result record building (incremental-save schema)."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from yoloeval.results import build_record  # noqa: E402


def _rec(**over):
    base = dict(
        benchmark="swebench_verified", config_name="c", instance_id="i",
        agent_config={}, resolved=False, metrics={}, patch="", session_log_path=None,
        eval_report={}, error=None, run_id="r", started_at="t",
    )
    base.update(over)
    return build_record(**base)


class BuildRecordTest(unittest.TestCase):
    def test_evaluated_defaults_true(self):
        self.assertTrue(_rec()["evaluated"])

    def test_evaluated_false_for_preliminary_save(self):
        # The runner persists pre-eval records as evaluated=False so a crash
        # leaves a clearly-unscored record that `eval` can pick up later.
        self.assertFalse(_rec(evaluated=False)["evaluated"])

    def test_record_carries_patch_and_metrics_for_later_scoring(self):
        r = _rec(evaluated=False, patch="diff --git ...", metrics={"cost_usd": 1.0})
        self.assertEqual(r["patch"], "diff --git ...")
        self.assertEqual(r["metrics"]["cost_usd"], 1.0)


if __name__ == "__main__":
    unittest.main()

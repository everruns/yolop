"""Tests for result record building (incremental-save schema)."""

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from yoloeval import results as results_mod  # noqa: E402
from yoloeval.results import (  # noqa: E402
    build_record,
    format_leaderboard,
    leaderboard,
    summarize,
)


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


def _write_run(cfg_dir: Path, iid: str, *, resolved: bool, cost: float) -> None:
    cfg_dir.mkdir(parents=True, exist_ok=True)
    rec = _rec(
        config_name=cfg_dir.name,
        instance_id=iid,
        resolved=resolved,
        metrics={"cost_usd": cost, "stop_reason": "completed"},
    )
    (cfg_dir / f"{iid}.json").write_text(json.dumps(rec) + "\n", encoding="utf-8")


class EfficiencyTest(unittest.TestCase):
    def _summarize(self, runs):
        """Summarize a config with the given (resolved, cost) runs in a temp dir."""
        with tempfile.TemporaryDirectory() as d:
            with mock.patch.object(results_mod, "RESULTS_DIR", Path(d)):
                cfg_dir = Path(d) / "bench" / "cfg"
                for i, (resolved, cost) in enumerate(runs):
                    _write_run(cfg_dir, f"i{i}", resolved=resolved, cost=cost)
                return summarize("bench", "cfg")

    def test_resolved_per_usd_is_score_over_cost(self):
        # 3 resolved of 4, $2 total -> 1.5 resolved per dollar.
        s = self._summarize([(True, 0.5), (True, 0.5), (True, 0.5), (False, 0.5)])
        self.assertEqual(s["efficiency"]["resolved_per_usd"], 1.5)
        # $2 / 3 resolved.
        self.assertAlmostEqual(s["efficiency"]["cost_per_resolved_usd"], 0.6667, places=3)

    def test_cost_per_resolved_none_when_nothing_resolved(self):
        s = self._summarize([(False, 1.0), (False, 1.0)])
        self.assertEqual(s["efficiency"]["resolved_per_usd"], 0.0)
        self.assertIsNone(s["efficiency"]["cost_per_resolved_usd"])

    def test_zero_cost_does_not_divide_by_zero(self):
        s = self._summarize([(True, 0.0)])
        self.assertEqual(s["efficiency"]["resolved_per_usd"], 0.0)


class LeaderboardTest(unittest.TestCase):
    def test_ranks_and_normalizes_to_least_efficient_baseline(self):
        with tempfile.TemporaryDirectory() as d:
            with mock.patch.object(results_mod, "RESULTS_DIR", Path(d)):
                # cheap: 2 resolved / $1 = 2.0 per $. pricey: 2 resolved / $4 = 0.5 per $.
                for i in range(2):
                    _write_run(Path(d) / "bench" / "cheap", f"i{i}", resolved=True, cost=0.5)
                for i in range(2):
                    _write_run(Path(d) / "bench" / "pricey", f"i{i}", resolved=True, cost=2.0)
                summarize("bench", "cheap")
                summarize("bench", "pricey")

                board = leaderboard("bench")
                # Most cost-efficient first; baseline is the least efficient.
                self.assertEqual([r["config_name"] for r in board["rows"]], ["cheap", "pricey"])
                self.assertEqual(board["baseline"], "pricey")
                by = {r["config_name"]: r for r in board["rows"]}
                self.assertEqual(by["pricey"]["value_multiple"], 1.0)
                self.assertEqual(by["cheap"]["value_multiple"], 4.0)  # 2.0 / 0.5
                # The table renders without error and shows the multiple.
                self.assertIn("4.00x", format_leaderboard(board))

    def test_empty_when_no_results(self):
        with tempfile.TemporaryDirectory() as d:
            with mock.patch.object(results_mod, "RESULTS_DIR", Path(d)):
                board = leaderboard("bench")
                self.assertEqual(board["rows"], [])
                self.assertIn("no results", format_leaderboard(board))


if __name__ == "__main__":
    unittest.main()
